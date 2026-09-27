//! Checksums the user expects for a download (published next to it by its site): MD5, SHA-1,
//! SHA-256 or SHA-512, recognised by length or by an `algo:` prefix; checked once the file is
//! complete.

use std::{io::Read, path::Path, sync::Arc};

use domain::{DownloadId, Status};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use super::{Manager, Verify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl Algo {
    const fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }

    fn from_len(len: usize) -> Option<Self> {
        match len {
            32 => Some(Self::Md5),
            40 => Some(Self::Sha1),
            64 => Some(Self::Sha256),
            128 => Some(Self::Sha512),
            _ => None,
        }
    }
}

/// `input` as the user pasted it (`SHA256: AB CD…`, `md5:…`, a bare hex string, `hash  file.iso`)
/// → canonical `algo:hex`; `None` when it is not a checksum.
pub fn parse(input: &str) -> Option<String> {
    let s = input.trim();
    let (label, rest) = match s.split_once(':') {
        Some((label, rest)) if label.len() <= 8 && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => {
            (Some(label.to_ascii_lowercase().replace('-', "")), rest)
        }
        _ => (None, s),
    };
    // `sha256sum` output: the hash, then the file name.
    let hex: String = rest.split_whitespace().take_while(|w| w.chars().all(|c| c.is_ascii_hexdigit())).collect::<String>().to_ascii_lowercase();
    let algo = Algo::from_len(hex.len())?;
    if label.is_some_and(|l| l != algo.name()) {
        return None; // "sha256:" with 32 hex digits: a typo, not an MD5
    }
    Some(format!("{}:{hex}", algo.name()))
}

fn split(checksum: &str) -> Option<(Algo, &str)> {
    let (name, hex) = checksum.split_once(':')?;
    let algo = [Algo::Md5, Algo::Sha1, Algo::Sha256, Algo::Sha512].into_iter().find(|a| a.name() == name)?;
    Some((algo, hex))
}

/// Hex digest of the file at `path` (read in 1 MiB blocks: any size, little memory).
pub fn digest(path: &Path, algo: Algo) -> std::io::Result<String> {
    fn run<D: Digest>(path: &Path) -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let mut hasher = D::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match file.read(&mut buf)? {
                0 => break,
                n => hasher.update(&buf[..n]),
            }
        }
        Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
    match algo {
        Algo::Md5 => run::<Md5>(path),
        Algo::Sha1 => run::<Sha1>(path),
        Algo::Sha256 => run::<Sha256>(path),
        Algo::Sha512 => run::<Sha512>(path),
    }
}

impl Manager {
    /// Sets (or clears) the checksum `id` must match; a finished file is checked at once.
    /// `false` when `input` is not a checksum.
    pub fn set_checksum(self: &Arc<Self>, id: DownloadId, input: &str) -> bool {
        let checksum = if input.trim().is_empty() {
            None
        } else {
            match parse(input) {
                Some(c) => Some(c),
                None => return false,
            }
        };
        let done = self.update(id, |e| {
            e.download.checksum = checksum;
            e.verify = Verify::None;
            *e.download.status() == Status::Completed
        });
        if done == Some(true) {
            self.verify(id);
        }
        true
    }

    /// Checks a finished download against its expected checksum, off the UI thread.
    pub(super) fn verify(self: &Arc<Self>, id: DownloadId) {
        let job = self.update_quiet(id, |e| {
            let (algo, hex) = split(e.download.checksum.as_deref()?)?;
            e.verify = Verify::Running;
            Some((e.download.target.clone(), algo, hex.to_owned(), e.name.clone()))
        });
        let Some(Some((path, algo, expected, name))) = job else { return };
        let this = self.clone();
        self.rt.spawn_blocking(move || {
            let digest = digest(&path, algo);
            // Worth keeping: VirusTotal looks files up by SHA-256.
            if algo == Algo::Sha256
                && let Ok(hash) = &digest
            {
                let hash = hash.clone();
                this.update_quiet(id, |e| e.sha256 = Some(hash));
            }
            let state = match digest {
                Ok(actual) if actual == expected => Verify::Ok,
                Ok(actual) => {
                    crate::notify::checksum_mismatch(&name);
                    Verify::Mismatch(actual)
                }
                Err(e) => Verify::Failed(e.to_string()),
            };
            this.update(id, |e| e.verify = state);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_what_users_paste() {
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(parse(sha256).unwrap(), format!("sha256:{sha256}"));
        assert_eq!(parse(&format!("SHA-256: {}", sha256.to_uppercase())).unwrap(), format!("sha256:{sha256}"));
        assert_eq!(parse(&format!("{sha256}  ubuntu.iso")).unwrap(), format!("sha256:{sha256}"));
        assert_eq!(parse("MD5:d41d8cd98f00b204e9800998ecf8427e").unwrap(), "md5:d41d8cd98f00b204e9800998ecf8427e");
        assert!(parse("sha256:d41d8cd98f00b204e9800998ecf8427e").is_none(), "label and length disagree");
        assert!(parse("not a hash").is_none());
        assert!(parse("abc").is_none());
    }

    #[test]
    fn digests_of_known_content() {
        let path = std::env::temp_dir().join(format!("rdm-digest-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(digest(&path, Algo::Md5).unwrap(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(digest(&path, Algo::Sha1).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(digest(&path, Algo::Sha256).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(digest(&path, Algo::Sha512).unwrap().starts_with("ddaf35a193617aba"));
        let _ = std::fs::remove_file(path);
    }
}
