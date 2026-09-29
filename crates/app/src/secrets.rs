//! Passwords — site logins, the proxy's, the VirusTotal API key — kept apart from `settings.json`:
//! on Windows encrypted with DPAPI (only this Windows account can decrypt them), on Linux in a file
//! only the user can read (0600, like `~/.netrc`). Never shown in the list, never sent anywhere but
//! to their site.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::settings::Settings;

const FILE: &str = "secrets.bin";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Secrets {
    pub sites: Vec<SiteLogin>,
    pub proxy_password: String,
    /// The user's own (free) VirusTotal API key; empty = not set up.
    pub virustotal_key: String,
}

/// HTTP login for a site: sent (Basic) to `host` and its subdomains.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteLogin {
    pub host: String,
    pub user: String,
    pub password: String,
}

impl Secrets {
    pub fn load() -> Self {
        std::fs::read(crate::settings::config_file(FILE))
            .ok()
            .and_then(|sealed| imp::open(&sealed))
            .and_then(|plain| serde_json::from_slice(&plain).ok())
            .unwrap_or_default()
    }

    /// `false` when they could not be written.
    pub fn save(&self) -> bool {
        let Ok(plain) = serde_json::to_vec(self) else { return false };
        let Some(sealed) = imp::seal(&plain) else { return false };
        let path = crate::settings::config_file(FILE);
        if let Some(dir) = path.parent() {
            let _ = crate::settings::create_private_dir(dir);
        }
        let tmp = crate::settings::with_suffix(&path, ".tmp");
        crate::settings::write_private(&tmp, &sealed).and_then(|()| std::fs::rename(tmp, path)).is_ok()
    }

    /// RDM 0.3.7 and older kept the VirusTotal key in the settings, in clear: it moves here once
    /// stored (`store`), and only then leaves the settings. `true` when the settings changed.
    pub fn adopt_virustotal_key(&mut self, settings: &mut Settings, store: impl FnOnce(&Self) -> bool) -> bool {
        if settings.virustotal_key.is_empty() {
            return false;
        }
        if self.virustotal_key.is_empty() {
            self.virustotal_key = settings.virustotal_key.clone();
            if !store(self) {
                return false; // kept in the settings meanwhile: not lost
            }
        }
        settings.virustotal_key.clear();
        true
    }

    /// The login saved for `url`'s host (or a parent domain), the most specific first.
    pub fn login_for(&self, url: &Url) -> Option<&SiteLogin> {
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        self.sites
            .iter()
            .filter(|s| !s.user.is_empty())
            .filter_map(|s| {
                let pattern = normalize_host(&s.host);
                let matches = !pattern.is_empty() && (host == pattern || host.ends_with(&format!(".{pattern}")));
                matches.then_some((pattern.len(), s))
            })
            .max_by_key(|(specificity, _)| *specificity)
            .map(|(_, s)| s)
    }
}

/// `https://Example.com:8080/x` → `example.com`: what a user may paste as a site.
pub fn normalize_host(input: &str) -> String {
    let s = input.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.split(['/', '?', '#']).next().unwrap_or_default();
    let s = s.rsplit_once('@').map_or(s, |(_, host)| host);
    // `[::1]:80` → `[::1]` (as `Url::host_str` writes an IPv6 host); `host:port` → `host`.
    let s = if s.starts_with('[') { s.find(']').map_or(s, |end| &s[..=end]) } else { s.split(':').next().unwrap_or_default() };
    s.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData},
    };

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr().cast_mut() }
    }

    /// DPAPI output buffer → owned bytes, freed.
    unsafe fn take(out: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
        if out.pbData.is_null() {
            return Vec::new();
        }
        // SAFETY: DPAPI returned `cbData` bytes at `pbData` (not null), allocated with LocalAlloc.
        unsafe {
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData.cast());
            bytes
        }
    }

    pub fn seal(plain: &[u8]) -> Option<Vec<u8>> {
        let input = blob(plain);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: valid input blob for the call's duration; `out` receives a LocalAlloc buffer.
        let ok = unsafe {
            CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        // SAFETY: `out` was filled by a successful call.
        (ok != 0).then(|| unsafe { take(&out) })
    }

    pub fn open(sealed: &[u8]) -> Option<Vec<u8>> {
        let input = blob(sealed);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as in `seal`.
        let ok = unsafe {
            CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        // SAFETY: `out` was filled by a successful call.
        (ok != 0).then(|| unsafe { take(&out) })
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn seal(plain: &[u8]) -> Option<Vec<u8>> {
        Some(plain.to_vec())
    }

    pub fn open(sealed: &[u8]) -> Option<Vec<u8>> {
        Some(sealed.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_normalized() {
        assert_eq!(normalize_host("https://User@Files.Example.com:8443/a?b"), "files.example.com");
        assert_eq!(normalize_host(" example.com. "), "example.com");
        assert_eq!(normalize_host("[::1]:80"), "[::1]");
    }

    #[test]
    fn the_most_specific_login_wins_and_lookalikes_do_not_match() {
        let login = |host: &str, user: &str| SiteLogin { host: host.into(), user: user.into(), password: "p".into() };
        let s = Secrets { sites: vec![login("example.com", "a"), login("files.example.com", "b"), login("", "c")], ..Default::default() };
        let url = |u: &str| u.parse::<Url>().unwrap();
        assert_eq!(s.login_for(&url("https://files.example.com/x")).unwrap().user, "b");
        assert_eq!(s.login_for(&url("https://cdn.example.com/x")).unwrap().user, "a");
        assert!(s.login_for(&url("https://notexample.com/x")).is_none());
        assert!(s.login_for(&url("https://example.com.evil.io/x")).is_none());
        // Written with its scheme, the more specific site still wins.
        let s = Secrets { sites: vec![login("http://files.example.com", "b"), login("example.com", "a")], ..Default::default() };
        assert_eq!(s.login_for(&url("http://files.example.com/x")).unwrap().user, "b");
    }

    #[test]
    fn the_virustotal_key_leaves_the_settings_once_stored() {
        let key = "a".repeat(64);
        let with_key = || Settings { virustotal_key: key.clone(), ..Settings::default() };
        let (mut settings, mut secrets) = (with_key(), Secrets::default());
        assert!(!secrets.adopt_virustotal_key(&mut settings, |_| false), "not stored: stays where it was");
        assert_eq!(settings.virustotal_key, key);
        let mut settings = with_key();
        assert!(Secrets::default().adopt_virustotal_key(&mut settings, |s| s.virustotal_key == key));
        assert!(settings.virustotal_key.is_empty());
        assert!(!serde_json::to_string(&settings).unwrap().contains("virustotal_key"), "no longer written in clear");
        // Already moved (an older RDM ran again meanwhile): the settings' copy just goes.
        let (mut settings, mut secrets) = (with_key(), Secrets { virustotal_key: "b".repeat(64), ..Secrets::default() });
        assert!(secrets.adopt_virustotal_key(&mut settings, |_| panic!("nothing to store")));
        assert_eq!(secrets.virustotal_key, "b".repeat(64));
    }

    #[test]
    fn sealing_round_trips() {
        let plain = b"{\"proxy_password\":\"s3cret\"}";
        let sealed = imp::seal(plain).unwrap();
        if cfg!(windows) {
            assert!(!sealed.windows(6).any(|w| w == b"s3cret"), "not stored in clear");
        }
        assert_eq!(imp::open(&sealed).unwrap(), plain);
    }
}
