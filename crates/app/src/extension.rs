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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Browser {
    Firefox,
    Waterfox,
    Chrome,
    Brave,
    Opera,
    Edge,
    Chromium,
}

/// Chromium-based browsers share one build; Firefox and its derivatives have their own manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Chromium,
    Firefox,
}

impl Browser {
    pub const ALL: [Self; 7] = [Self::Firefox, Self::Waterfox, Self::Chrome, Self::Brave, Self::Opera, Self::Edge, Self::Chromium];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Firefox => "Firefox",
            Self::Waterfox => "Waterfox",
            Self::Chrome => "Google Chrome",
            Self::Brave => "Brave",
            Self::Opera => "Opera",
            Self::Edge => "Microsoft Edge",
            Self::Chromium => "Chromium",
        }
    }

    /// What the extension reports about itself (`x-rdm-browser`, see `extension/background.js`).
    pub const fn key(self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::Waterfox => "waterfox",
            Self::Chrome => "chrome",
            Self::Brave => "brave",
            Self::Opera => "opera",
            Self::Edge => "edge",
            Self::Chromium => "chromium",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.key() == key)
    }

    pub const fn flavour(self) -> Flavour {
        match self {
            Self::Firefox | Self::Waterfox => Flavour::Firefox,
            _ => Flavour::Chromium,
        }
    }

    /// Where extensions are managed (opened for the user, who confirms the installation there).
    pub const fn extensions_page(self) -> &'static str {
        match self {
            Self::Firefox | Self::Waterfox => "about:debugging#/runtime/this-firefox",
            Self::Brave => "brave://extensions/",
            Self::Opera => "opera://extensions/",
            Self::Edge => "edge://extensions/",
            Self::Chrome | Self::Chromium => "chrome://extensions/",
        }
    }

    /// The browser's executable, if it is installed.
    pub fn find(self) -> Option<PathBuf> {
        imp::find(self)
    }
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

/// The version in the bundled manifest.
pub fn version() -> String {
    FILES
        .iter()
        .find(|(name, _)| *name == "manifest.json")
        .and_then(|(_, bytes)| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|m| m.get("version")?.as_str().map(str::to_owned))
        .unwrap_or_default()
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
fn zip<'a>(files: impl Iterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
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

    use super::Browser;

    pub fn find(browser: Browser) -> Option<PathBuf> {
        let (exe, known): (&str, &[&str]) = match browser {
            Browser::Firefox => ("firefox.exe", &[r"Mozilla Firefox\firefox.exe"]),
            Browser::Waterfox => (
                "waterfox.exe",
                &[r"Waterfox\waterfox.exe", r"Waterfox Current\waterfox.exe", r"Waterfox Classic\waterfox.exe", r"Programs\Waterfox\waterfox.exe"],
            ),
            Browser::Chrome => ("chrome.exe", &[r"Google\Chrome\Application\chrome.exe"]),
            Browser::Brave => ("brave.exe", &[r"BraveSoftware\Brave-Browser\Application\brave.exe"]),
            Browser::Opera => ("opera.exe", &[r"Programs\Opera\opera.exe", r"Programs\Opera\launcher.exe", r"Opera\launcher.exe"]),
            Browser::Edge => ("msedge.exe", &[r"Microsoft\Edge\Application\msedge.exe"]),
            Browser::Chromium => ("chromium.exe", &[r"Chromium\Application\chrome.exe"]),
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

    use super::Browser;

    pub fn find(browser: Browser) -> Option<PathBuf> {
        let names: &[&str] = match browser {
            Browser::Firefox => &["firefox", "firefox-esr", "org.mozilla.firefox"],
            Browser::Waterfox => &["waterfox", "waterfox-g", "waterfox-current", "net.waterfox.waterfox"],
            Browser::Chrome => &["google-chrome", "google-chrome-stable", "com.google.Chrome"],
            Browser::Brave => &["brave-browser", "brave", "com.brave.Browser"],
            Browser::Opera => &["opera", "com.opera.Opera"],
            Browser::Edge => &["microsoft-edge", "microsoft-edge-stable", "com.microsoft.Edge"],
            Browser::Chromium => &["chromium", "chromium-browser", "org.chromium.Chromium"],
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

    #[test]
    fn keys_round_trip() {
        for b in Browser::ALL {
            assert_eq!(Browser::from_key(b.key()), Some(b));
        }
        assert_eq!(Browser::from_key("netscape"), None);
    }
}
