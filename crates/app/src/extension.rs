//! The browser extension, bundled in the executable. RDM writes it to a stable folder the browsers
//! load it from — Chrome, Brave, Opera, Edge, Chromium: "load unpacked"; Firefox: the signed
//! package of the GitHub release when there is one, a temporary add-on otherwise; Waterfox (a
//! Firefox derivative that can accept unsigned packages): the package itself — rewrites that copy
//! when a newer RDM brings a newer extension, and opens each browser where the user confirms.

use std::{
    borrow::Cow,
    fs, io,
    path::{Path, PathBuf},
};

include!(concat!(env!("OUT_DIR"), "/extension_files.rs"));

/// A browser the extension can run in: a well-known one, one Windows lists as installed, one the
/// user added, or one the extension reported from. Every browser belongs to one of two families
/// (`Flavour`): what works in Chrome works in any Chromium-based browser, what works in Firefox in
/// any of its derivatives, so none has to be listed here to be supported.
#[derive(Debug, Clone, Copy)]
pub struct Browser(&'static Info);

#[derive(Debug)]
struct Info {
    /// What the extension reports about itself (`x-rdm-browser`): see `key_of`.
    key: String,
    name: String,
    flavour: Flavour,
    /// Where it was found (Windows' list of browsers, or picked by the user).
    exe: Option<PathBuf>,
}

/// Chromium-based browsers share one build; Firefox and its derivatives have their own manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavour {
    Chromium,
    Firefox,
}

/// Browsers known by name: the name shown and the family. Any other browser works the same,
/// named as Windows or the extension names it.
const KNOWN: &[(&str, &str, Flavour)] = &[
    ("firefox", "Firefox", Flavour::Firefox),
    ("waterfox", "Waterfox", Flavour::Firefox),
    ("librewolf", "LibreWolf", Flavour::Firefox),
    ("floorp", "Floorp", Flavour::Firefox),
    ("zen", "Zen Browser", Flavour::Firefox),
    ("mullvad", "Mullvad Browser", Flavour::Firefox),
    ("chrome", "Google Chrome", Flavour::Chromium),
    ("edge", "Microsoft Edge", Flavour::Chromium),
    ("brave", "Brave", Flavour::Chromium),
    ("opera", "Opera", Flavour::Chromium),
    ("vivaldi", "Vivaldi", Flavour::Chromium),
    ("chromium", "Chromium", Flavour::Chromium),
];

/// Every browser met so far, one entry per key: `Browser` stays a cheap copy.
static SEEN: std::sync::Mutex<Vec<&'static Info>> = std::sync::Mutex::new(Vec::new());

/// The key of a browser's name, as the extension computes it too (`browserKey` in
/// `extension/shared.js`, same tests): the first word that is not the vendor's or a generic one,
/// letters only, at most 16. "Google Chrome" is `chrome`, "Mullvad Browser" is `mullvad`.
pub fn key_of(name: &str) -> String {
    const SKIP: [&str; 7] = ["mozilla", "google", "microsoft", "browser", "stable", "web", "the"];
    name.split(|c: char| !c.is_ascii_alphabetic())
        .map(str::to_ascii_lowercase)
        .find(|w| !w.is_empty() && !SKIP.contains(&w.as_str()))
        .map(|w| w.chars().take(16).collect())
        .unwrap_or_default()
}

pub(crate) fn valid_key(key: &str) -> bool {
    (1..=16).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase())
}

impl PartialEq for Browser {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}

impl Eq for Browser {}

impl std::hash::Hash for Browser {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.key.hash(state);
    }
}

impl Browser {
    /// The browser of this key; learnt with `exe` (and the name Windows gives it) when found.
    fn with(key: &str, name: Option<&str>, flavour: Option<Flavour>, exe: Option<PathBuf>) -> Self {
        let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = seen.iter().position(|i| i.key == key);
        if let Some(i) = old
            && (exe.is_none() || seen[i].exe == exe)
        {
            return Self(seen[i]);
        }
        let old = old.map(|i| seen.remove(i));
        let known = KNOWN.iter().find(|(k, ..)| *k == key);
        // Leaked on purpose: one per browser (and per new executable), a handful in all.
        let info: &'static Info = Box::leak(Box::new(Info {
            key: key.to_owned(),
            // A well-known name first ("Google Chrome", not Windows' "Chrome").
            name: known
                .map(|k| k.1.to_owned())
                .or_else(|| name.map(str::to_owned))
                .or_else(|| old.map(|o| o.name.clone()))
                .unwrap_or_else(|| title(key)),
            flavour: known.map(|k| k.2).or(flavour).or(old.map(|o| o.flavour)).unwrap_or(Flavour::Chromium),
            exe: exe.or_else(|| old.and_then(|o| o.exe.clone())),
        }));
        seen.push(info);
        Self(info)
    }

    /// The browser the extension says it runs in (`x-rdm-browser`); `None` for a malformed key.
    /// One RDM does not know otherwise is taken as Chromium-based until found on this computer.
    pub fn from_key(key: &str) -> Option<Self> {
        valid_key(key).then(|| Self::with(key, None, None, None))
    }

    pub fn name(self) -> &'static str {
        &self.0.name
    }

    pub fn key(self) -> &'static str {
        &self.0.key
    }

    pub fn flavour(self) -> Flavour {
        self.0.flavour
    }

    /// Where extensions are managed (opened for the user, who confirms the installation there).
    /// Chromium-based browsers without a page of their own accept `chrome://extensions`.
    pub fn extensions_page(self) -> &'static str {
        match (self.flavour(), self.key()) {
            (Flavour::Firefox, _) => "about:debugging#/runtime/this-firefox",
            (_, "brave") => "brave://extensions/",
            (_, "opera") => "opera://extensions/",
            (_, "edge") => "edge://extensions/",
            (_, "vivaldi") => "vivaldi://extensions/",
            _ => "chrome://extensions/",
        }
    }

    /// The browser's executable, if it is installed.
    pub fn find(self) -> Option<PathBuf> {
        self.0.exe.clone().filter(|p| p.is_file()).or_else(|| imp::find(self.key()))
    }

    /// The browser at `exe` (added by the user): its family from its files, named after the file.
    /// `None`: not a browser the extension can run in.
    pub fn at(exe: &Path) -> Option<Self> {
        let flavour = flavour_of(exe)?;
        let stem = exe.file_stem()?.to_string_lossy().into_owned();
        let key = key_of(&stem);
        valid_key(&key).then(|| Self::with(&key, Some(&title(&stem)), Some(flavour), Some(exe.to_path_buf())))
    }
}

/// "waterfox" gives "Waterfox".
fn title(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// A browser's family, from the files beside its executable: Firefox and every derivative ship
/// `xul.dll` (`libxul.so`); Chromium-based browsers `.pak` resources (in a version sub-folder on
/// Windows). `None`: not a browser the extension can run in.
pub fn flavour_of(exe: &Path) -> Option<Flavour> {
    let dir = exe.parent()?;
    let names = |dir: &Path| -> Vec<String> {
        fs::read_dir(dir).map(|e| e.flatten().map(|e| e.file_name().to_string_lossy().to_ascii_lowercase()).collect()).unwrap_or_default()
    };
    let here = names(dir);
    if here.iter().any(|n| n == "xul.dll" || n == "libxul.so") {
        return Some(Flavour::Firefox);
    }
    let chromium = |list: &[String]| list.iter().any(|n| n.ends_with(".pak") || n == "chrome.dll" || n == "msedge.dll");
    let versions = fs::read_dir(dir).map(|e| e.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect::<Vec<_>>()).unwrap_or_default();
    (chromium(&here) || versions.iter().any(|v| chromium(&names(v)))).then_some(Flavour::Chromium)
}

/// The browsers of this computer: those Windows lists as installed (any browser, known here or
/// not), the well-known ones found where they usually are, and those the user added (`custom`).
/// One browser per key, the first found: two of the same key (Firefox and Firefox Developer
/// Edition are both `firefox`) would otherwise take each other's place at every call, each time a
/// new (leaked) `Info`.
pub fn installed(custom: &[PathBuf]) -> Vec<Browser> {
    let mut list: Vec<Browser> = Vec::new();
    let listed = |list: &[Browser], key: &str| list.iter().any(|b| b.key() == key);
    for (name, exe) in imp::registered() {
        let key = key_of(&name);
        if valid_key(&key)
            && !listed(&list, &key)
            && let Some(flavour) = flavour_of(&exe)
        {
            list.push(Browser::with(&key, Some(&name), Some(flavour), Some(exe)));
        }
    }
    for (key, ..) in KNOWN {
        if !listed(&list, key) {
            let b = Browser::with(key, None, None, None);
            if b.find().is_some() {
                list.push(b);
            }
        }
    }
    for exe in custom {
        let key = exe.file_stem().map(|s| key_of(&s.to_string_lossy())).unwrap_or_default();
        if !listed(&list, &key)
            && let Some(b) = Browser::at(exe)
        {
            list.push(b);
        }
    }
    list
}

impl Flavour {
    const fn dir_name(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
        }
    }
}

/// Parent of the unpacked copies. Visible on Linux: browsers packaged as Snap or Flatpak can only
/// read non-hidden folders of the home directory.
pub fn base() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map_or_else(std::env::temp_dir, PathBuf::from).join("RDM").join("Extension")
    }
    #[cfg(not(windows))]
    {
        directories::BaseDirs::new()
            .map_or_else(std::env::temp_dir, |d| d.home_dir().to_path_buf())
            .join("RDM")
            .join("Extension")
    }
}

/// The folder a browser loads the unpacked extension from.
pub fn folder(flavour: Flavour) -> PathBuf {
    base().join(flavour.dir_name())
}

/// The extension's files for `flavour`: identical except the Firefox manifest.
pub fn files(flavour: Flavour) -> Vec<(&'static str, Cow<'static, [u8]>)> {
    FILES
        .iter()
        .map(|&(name, bytes)| match (name, flavour) {
            ("manifest.json", Flavour::Firefox) => (name, Cow::Owned(firefox_manifest(bytes))),
            _ => (name, Cow::Borrowed(bytes)),
        })
        .collect()
}

/// Same derivation as `packaging/firefox/build.sh`: Chrome wants an MV3 background service worker
/// (and flags `background.scripts`), Firefox only knows `background.scripts`; Chrome-only keys go;
/// `webRequestBlocking` (refused by Chrome MV3) lets Firefox take a download over before it starts.
fn firefox_manifest(chrome: &[u8]) -> Vec<u8> {
    let mut manifest: serde_json::Value = serde_json::from_slice(chrome).expect("extension/manifest.json is valid JSON");
    if let Some(m) = manifest.as_object_mut() {
        m.remove("key");
        m.remove("minimum_chrome_version");
        if let Some(bg) = m.get_mut("background").and_then(serde_json::Value::as_object_mut)
            && let Some(worker) = bg.remove("service_worker")
        {
            bg.insert("scripts".into(), serde_json::Value::Array(vec![worker]));
        }
        if let Some(permissions) = m.get_mut("permissions").and_then(serde_json::Value::as_array_mut) {
            permissions.push("webRequestBlocking".into());
        }
    }
    serde_json::to_vec_pretty(&manifest).expect("serializable")
}

/// The version in the bundled manifest (read once: the extension window shows it at every frame).
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        FILES
            .iter()
            .find(|(name, _)| *name == "manifest.json")
            .and_then(|(_, bytes)| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .and_then(|m| m.get("version")?.as_str().map(str::to_owned))
            .unwrap_or_default()
    })
}

/// Writes (or brings up to date) the unpacked extension; returns its folder. Files already
/// identical are left alone.
pub fn write(flavour: Flavour) -> io::Result<PathBuf> {
    let dir = folder(flavour);
    for (name, bytes) in files(flavour) {
        let path = dir.join(name);
        if fs::read(&path).is_ok_and(|old| old == *bytes) {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = crate::settings::with_suffix(&path, ".tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(tmp, path)?;
    }
    Ok(dir)
}

/// At start: copies installed from an older RDM get this version's files (browsers load them
/// at their next start). Folders never installed are not created.
pub fn refresh_installed() {
    for flavour in [Flavour::Chromium, Flavour::Firefox] {
        if folder(flavour).join("manifest.json").exists() {
            let _ = write(flavour);
        }
    }
    if base().join(XPI).exists() {
        let _ = write_xpi();
    }
}

const XPI: &str = "rdm-firefox.xpi";

/// The extension's page in the Firefox store (same ID as the bundled package).
pub const FIREFOX_STORE: &str = "https://addons.mozilla.org/firefox/addon/rdm-rust-download-manager/";

/// The Firefox package as one `.xpi` file (unsigned): permanent installation in Firefox Developer
/// Edition, Nightly, ESR (with `xpinstall.signatures.required` off) and derivatives such as
/// LibreWolf; release Firefox only installs signed packages.
pub fn write_xpi() -> io::Result<PathBuf> {
    let path = base().join(XPI);
    fs::create_dir_all(base())?;
    let files = files(Flavour::Firefox);
    let bytes = zip(files.iter().map(|(n, b)| (*n, b.as_ref())));
    if fs::read(&path).is_ok_and(|old| old == bytes) {
        return Ok(path);
    }
    let tmp = crate::settings::with_suffix(&path, ".tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, &path)?;
    Ok(path)
}

/// Opens `target` (a page or a package) in the browser whose executable is `exe`.
pub fn launch(exe: &Path, target: &str) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe).arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    // Reaped off the UI thread: the browser may run for hours (Linux would keep a zombie otherwise).
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A minimal ZIP archive (stored, no compression): enough for an `.xpi`, no extra dependency.
pub(crate) fn zip<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
    let (mut out, mut central, mut count) = (Vec::new(), Vec::new(), 0u16);
    for (name, data) in files {
        let (crc, size, offset) = (crc32(data), data.len() as u32, out.len() as u32);
        let header = |sig: u32, central: bool| {
            let mut h = Vec::new();
            h.extend_from_slice(&sig.to_le_bytes());
            if central {
                h.extend_from_slice(&20u16.to_le_bytes()); // made by
            }
            // version needed, flags (UTF-8 names), method (stored), time, date (1980-01-01)
            for v in [20u16, 0x0800, 0, 0, 0x21] {
                h.extend_from_slice(&v.to_le_bytes());
            }
            for v in [crc, size, size] {
                h.extend_from_slice(&v.to_le_bytes());
            }
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // extra
            if central {
                for v in [0u16, 0, 0] {
                    h.extend_from_slice(&v.to_le_bytes()); // comment, disk, internal attributes
                }
                h.extend_from_slice(&0u32.to_le_bytes()); // external attributes
                h.extend_from_slice(&offset.to_le_bytes());
            }
            h.extend_from_slice(name.as_bytes());
            h
        };
        out.extend(header(0x0403_4b50, false));
        out.extend_from_slice(data);
        central.extend(header(0x0201_4b50, true));
        count += 1;
    }
    let (dir_offset, dir_size) = (out.len() as u32, central.len() as u32);
    out.extend(central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    for v in [0u16, 0, count, count] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&dir_size.to_le_bytes());
    out.extend_from_slice(&dir_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| if c & 1 == 1 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 })
    })
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
    };

    /// Browsers Windows lists as installed (Default apps): name and executable. Every browser
    /// installer registers there, known to RDM or not.
    pub fn registered() -> Vec<(String, PathBuf)> {
        let mut list = Vec::new();
        for (hive, path) in [
            (HKEY_CURRENT_USER, r"SOFTWARE\Clients\StartMenuInternet"),
            (HKEY_LOCAL_MACHINE, r"SOFTWARE\Clients\StartMenuInternet"),
            (HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Clients\StartMenuInternet"),
        ] {
            let Ok(clients) = RegKey::predef(hive).open_subkey(path) else { continue };
            for id in clients.enum_keys().flatten() {
                let Ok(client) = clients.open_subkey(&id) else { continue };
                let name: String = client.get_value("").unwrap_or_else(|_| id.clone());
                let Some(exe) = client.open_subkey(r"shell\open\command").ok().and_then(|c| c.get_value::<String, _>("").ok()) else { continue };
                // `"C:\…\browser.exe" --args` or an unquoted path.
                let exe = exe.strip_prefix('"').and_then(|e| e.split('"').next()).unwrap_or(exe.split(" -").next().unwrap_or(&exe)).trim();
                let exe = PathBuf::from(exe);
                if exe.is_file() && !list.iter().any(|(_, e)| e == &exe) {
                    list.push((name, exe));
                }
            }
        }
        list
    }

    /// A well-known browser's executable, where its installer usually puts it.
    pub fn find(key: &str) -> Option<PathBuf> {
        let (exe, known): (&str, &[&str]) = match key {
            "firefox" => ("firefox.exe", &[r"Mozilla Firefox\firefox.exe"]),
            "waterfox" => (
                "waterfox.exe",
                &[r"Waterfox\waterfox.exe", r"Waterfox Current\waterfox.exe", r"Waterfox Classic\waterfox.exe", r"Programs\Waterfox\waterfox.exe"],
            ),
            "chrome" => ("chrome.exe", &[r"Google\Chrome\Application\chrome.exe"]),
            "brave" => ("brave.exe", &[r"BraveSoftware\Brave-Browser\Application\brave.exe"]),
            "opera" => ("opera.exe", &[r"Programs\Opera\opera.exe", r"Programs\Opera\launcher.exe", r"Opera\launcher.exe"]),
            "edge" => ("msedge.exe", &[r"Microsoft\Edge\Application\msedge.exe"]),
            "chromium" => ("chromium.exe", &[r"Chromium\Application\chrome.exe"]),
            "vivaldi" => ("vivaldi.exe", &[r"Vivaldi\Application\vivaldi.exe"]),
            "librewolf" => ("librewolf.exe", &[r"LibreWolf\librewolf.exe"]),
            _ => return None,
        };
        let registered = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().find_map(|hive| {
            let key = RegKey::predef(hive).open_subkey(format!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe}")).ok()?;
            let path: String = key.get_value("").ok()?;
            Some(PathBuf::from(path.trim_matches('"')))
        });
        registered.filter(|p| p.is_file()).or_else(|| {
            ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
                .into_iter()
                .filter_map(std::env::var_os)
                .flat_map(|root| known.iter().map(move |k| PathBuf::from(&root).join(k)))
                .find(|p| p.is_file())
        })
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    /// Linux has no list of installed browsers: the well-known ones are looked for by name.
    pub fn registered() -> Vec<(String, PathBuf)> {
        Vec::new()
    }

    pub fn find(key: &str) -> Option<PathBuf> {
        let names: &[&str] = match key {
            "firefox" => &["firefox", "firefox-esr", "org.mozilla.firefox"],
            "waterfox" => &["waterfox", "waterfox-g", "waterfox-current", "net.waterfox.waterfox"],
            "librewolf" => &["librewolf", "io.gitlab.librewolf-community"],
            "floorp" => &["floorp", "one.ablaze.floorp"],
            "zen" => &["zen", "zen-browser", "app.zen_browser.zen"],
            "mullvad" => &["mullvad-browser", "net.mullvad.MullvadBrowser"],
            "chrome" => &["google-chrome", "google-chrome-stable", "com.google.Chrome"],
            "brave" => &["brave-browser", "brave", "com.brave.Browser"],
            "opera" => &["opera", "com.opera.Opera"],
            "edge" => &["microsoft-edge", "microsoft-edge-stable", "com.microsoft.Edge"],
            "vivaldi" => &["vivaldi", "vivaldi-stable", "com.vivaldi.Vivaldi"],
            "chromium" => &["chromium", "chromium-browser", "org.chromium.Chromium"],
            _ => return None,
        };
        let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
        let dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            // Waterfox's own tarball unpacks to /opt/waterfox.
            .chain(["/snap/bin", "/var/lib/flatpak/exports/bin", "/opt/waterfox"].map(PathBuf::from))
            .chain(home.map(|h| h.join(".local/share/flatpak/exports/bin")))
            .collect();
        names.iter().flat_map(|n| dirs.iter().map(move |d| d.join(n))).find(|p| p.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_the_whole_extension() {
        for name in ["manifest.json", "background.js", "content.js", "shared.js", "relay.html", "icons/icon128.png"] {
            assert!(FILES.iter().any(|(n, _)| *n == name), "{name}");
        }
        assert!(!FILES.iter().any(|(n, _)| n.starts_with("test/")), "tests stay out");
        assert!(!version().is_empty());
    }

    #[test]
    fn firefox_manifest_is_derived_like_the_build_script() {
        let firefox = files(Flavour::Firefox);
        let manifest = &firefox.iter().find(|(n, _)| *n == "manifest.json").unwrap().1;
        let m: serde_json::Value = serde_json::from_slice(manifest).unwrap();
        assert_eq!(m["background"]["scripts"][0], "background.js");
        assert!(m["background"].get("service_worker").is_none());
        assert!(m.get("key").is_none() && m.get("minimum_chrome_version").is_none());
        assert!(m["browser_specific_settings"]["gecko"]["id"].is_string());
        assert!(m["permissions"].as_array().unwrap().iter().any(|p| p == "webRequestBlocking"));
        let chrome: serde_json::Value =
            serde_json::from_slice(&files(Flavour::Chromium).iter().find(|(n, _)| *n == "manifest.json").unwrap().1).unwrap();
        assert!(chrome["background"]["service_worker"].is_string() && chrome.get("key").is_some());
    }

    #[test]
    fn crc_and_zip_layout() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let z = zip([("a.txt", b"hi".as_slice())].into_iter());
        assert_eq!(&z[..4], b"PK\x03\x04");
        assert_eq!(&z[z.len() - 22..z.len() - 18], b"PK\x05\x06");
        assert_eq!(u16::from_le_bytes([z[z.len() - 12], z[z.len() - 11]]), 1, "one entry");
    }

    /// Same cases as `browserKey` in `extension/test/shared.test.mjs`.
    #[test]
    fn keys_of_browser_names() {
        for (name, key) in [
            ("Google Chrome", "chrome"),
            ("Microsoft Edge", "edge"),
            ("Mozilla Firefox", "firefox"),
            ("Firefox", "firefox"),
            ("Waterfox", "waterfox"),
            ("Mullvad Browser", "mullvad"),
            ("Zen Browser", "zen"),
            ("Opera Stable", "opera"),
            ("Brave", "brave"),
            ("Thorium", "thorium"),
            ("Supercalifragilisticexpialidocious", "supercalifragili"),
            ("", ""),
        ] {
            assert_eq!(key_of(name), key, "{name}");
        }
    }

    #[test]
    fn browsers_by_key() {
        let firefox = Browser::from_key("firefox").unwrap();
        assert_eq!((firefox.name(), firefox.flavour()), ("Firefox", Flavour::Firefox));
        let thorium = Browser::from_key("thorium").unwrap();
        assert_eq!((thorium.name(), thorium.flavour(), thorium.extensions_page()), ("Thorium", Flavour::Chromium, "chrome://extensions/"));
        assert_eq!(Browser::from_key("thorium"), Some(thorium), "the same browser again");
        assert!(Browser::from_key("Net Scape").is_none() && Browser::from_key("").is_none());
    }
}
