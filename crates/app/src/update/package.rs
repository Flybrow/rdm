//! Downloading an update package from GitHub and checking it: size, SHA-256, format and the
//! Ed25519 signature of the release workflow.

use super::*;


/// Public half of the key the release workflow signs packages with (Ed25519, raw 32 bytes).
const PUBLIC_KEY: [u8; 32] = hex32("a972e3ab906c230c0e1c96b750e0c7d2abeae96c5979f9c48085853506b9b135");

const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 64);
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

/// A package downloaded and checked, and the SHA-256 its signature covers: the installation
/// checks the file against it again, at the last moment (see `install`).
#[derive(Debug, Clone)]
pub struct Verified {
    pub path: PathBuf,
    pub sha256: String,
}

/// Downloads `package` of `version` and checks it (size, SHA-256, format, signature);
/// `progress` gets the fraction done.
pub async fn download(client: &reqwest::Client, version: &str, package: &Package, progress: impl Fn(f32)) -> Result<Verified, String> {
    if !trusted(&package.url) {
        return Err(tr!("adresse de téléchargement inattendue", "unexpected download address").into());
    }
    let Some(sig_url) = package.signature.as_deref().filter(|u| trusted(u)) else {
        return Err(tr!("mise à jour non signée : installation refusée", "unsigned update: installation refused").into());
    };
    let signature = fetch_small(client, sig_url).await?;
    let safe: String = version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')).collect();
    let extension = package.name.rsplit_once('.').map_or("bin", |(_, e)| e);
    let extension = if package.name.ends_with(".tar.gz") { "tar.gz" } else { extension };
    let dir = work_dir().map_err(|e| e.to_string())?;
    let path = dir.join(format!("rdm-update-{safe}.{extension}"));
    let mut last = String::new();
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let result = fetch(client, version, package, &path, &signature, &progress).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(crate::settings::with_suffix(&path, ".tmp")).await;
        }
        match result {
            Ok(sha256) => return Ok(Verified { path, sha256 }),
            Err(Fatal(reason)) => return Err(reason),
            Err(Retry(reason)) => last = reason,
        }
    }
    Err(last)
}

/// A small file (a signature), with retries.
async fn fetch_small(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    for attempt in 0..DOWNLOAD_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(2 << attempt)).await;
        }
        let res = client.get(url).timeout(Duration::from_secs(30)).send().await.and_then(reqwest::Response::error_for_status);
        if let Ok(res) = res
            && let Ok(bytes) = res.bytes().await
            && bytes.len() <= 1024
        {
            return Ok(bytes.to_vec());
        }
    }
    Err(tr!("signature de la mise à jour introuvable", "cannot fetch the update's signature").into())
}

enum Failure {
    Retry(String),
    Fatal(String),
}
use Failure::{Fatal, Retry};

/// The package at `path`, checked; its SHA-256.
async fn fetch(client: &reqwest::Client, version: &str, package: &Package, path: &Path, signature: &[u8], progress: &impl Fn(f32)) -> Result<String, Failure> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let res = client.get(&package.url).send().await.map_err(|_| Retry(tr!("GitHub est injoignable", "GitHub cannot be reached").into()))?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let reason = crate::trf!("téléchargement refusé par GitHub ({status})", "download refused by GitHub ({status})", status = status);
        return Err(if res.status().is_server_error() || status == 429 { Retry(reason) } else { Fatal(reason) });
    }
    let too_big = || Fatal(tr!("paquet anormalement gros", "abnormally large package").into());
    let total = if package.size > 0 { package.size } else { res.content_length().unwrap_or(0) };
    if total > MAX_PACKAGE {
        return Err(too_big());
    }
    let tmp = crate::settings::with_suffix(path, ".tmp");
    let io = |e: std::io::Error| Fatal(crate::trf!("écriture impossible : {e}", "cannot write: {e}", e = e));
    let mut file = tokio::fs::File::create(&tmp).await.map_err(io)?;
    // Streamed to disk: only the first bytes (the format) and the running hash stay in memory.
    let (mut stream, mut head, mut received, mut hash) = (res.bytes_stream(), Vec::with_capacity(16), 0u64, Sha256::new());
    loop {
        let chunk = match tokio::time::timeout(STALL, stream.next()).await {
            Err(_) => return Err(Retry(tr!("téléchargement bloqué (connexion)", "download stalled (connection)").into())),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(Retry(tr!("téléchargement interrompu", "download interrupted").into())),
            Ok(Some(Ok(chunk))) => chunk,
        };
        received += chunk.len() as u64;
        if received > MAX_PACKAGE {
            return Err(too_big());
        }
        if head.len() < 16 {
            head.extend_from_slice(&chunk[..chunk.len().min(16 - head.len())]);
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(io)?;
        if total > 0 {
            progress((received as f32 / total as f32).min(1.0));
        }
    }
    file.sync_all().await.map_err(io)?;
    drop(file);

    if total > 0 && received != total {
        return Err(Retry(tr!("paquet incomplet", "incomplete package").into()));
    }
    let digest = crate::manager::checksum::hex(&hash.finalize());
    if package.sha256.as_ref().is_some_and(|expected| *expected != digest) {
        return Err(Retry(tr!("paquet corrompu (empreinte SHA-256 différente)", "corrupted package (SHA-256 mismatch)").into()));
    }
    if !format_matches(&package.name, &head) {
        return Err(Retry(tr!("le fichier reçu n'est pas le paquet attendu", "the file received is not the expected package").into()));
    }
    if !signature_valid(signed_statement(version, &package.name, &digest).as_bytes(), signature) {
        return Err(Fatal(tr!("signature de la mise à jour invalide : installation refusée", "invalid update signature: installation refused").into()));
    }
    tokio::fs::rename(&tmp, path).await.map_err(io)?;
    Ok(digest)
}

/// The package's own format, from its first bytes.
fn format_matches(name: &str, data: &[u8]) -> bool {
    let name = name.to_ascii_lowercase();
    let magic: &[u8] = if name.ends_with(".msi") {
        &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1] // OLE compound file
    } else if name.ends_with(".tar.gz") {
        &[0x1F, 0x8B]
    } else if name.ends_with(".deb") {
        b"!<arch>\n"
    } else if name.ends_with(".rpm") {
        &[0xED, 0xAB, 0xEE, 0xDB]
    } else {
        return false;
    };
    data.starts_with(magic)
}

/// What the release workflow signs for each package (`.github/workflows/release.yml`): the version
/// it belongs to, its name and its SHA-256. Binding the version defeats replays: an older signed
/// package (with a since-fixed flaw) cannot be served again as a newer release.
fn signed_statement(version: &str, name: &str, sha256: &str) -> String {
    format!("rdm-update\nversion={version}\nname={name}\nsha256={sha256}\n")
}

/// Ed25519 signature of `message` by the release key.
fn signature_valid(message: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::{Signature, VerifyingKey};
    let (Ok(key), Ok(sig)) = (VerifyingKey::from_bytes(&PUBLIC_KEY), <[u8; 64]>::try_from(signature)) else { return false };
    key.verify_strict(message, &Signature::from_bytes(&sig)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release workflow checks its signing key against `PUBLIC_KEY` in this very file, by its
    /// path: moved without the script, every release was refused (0.3.9's first tag).
    #[test]
    fn the_release_workflow_looks_for_the_key_here() {
        let script = include_str!("../../../../.github/sign-packages.sh");
        let here = file!().replace('\\', "/");
        assert!(script.contains(&format!("grep -q \"\\\"$public\\\"\" {here}")), "sign-packages.sh must read {here}");
    }

    #[test]
    fn signatures_are_checked_against_the_release_key() {
        // Made with the release key exactly as the release workflow does (`openssl pkeyutl -sign
        // -rawin` over the statement).
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let hex = "58bc8644f48626024e184357d866862636c6c1e0a87bbfe45437383bae392645\
                   a728ed1a840668d9e9147528bead52304664bd6fd2199b86f8acb05717d23c04";
        let good: Vec<u8> = (0..64).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
        let statement = |version: &str, name: &str, sha: &str| signed_statement(version, name, sha).into_bytes();
        assert!(signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &good));
        assert!(!signature_valid(&statement("9.9.9", "RDM-0.3.0-x64-setup.msi", EMPTY), &good), "an old package replayed as newer");
        assert!(!signature_valid(&statement("0.3.0", "rdm_0.3.0-1_amd64.deb", EMPTY), &good), "another package");
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", &EMPTY.replace('e', "f")), &good), "other content");
        let mut forged = good.clone();
        forged[10] ^= 1;
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &forged), "a forged signature");
        assert!(!signature_valid(&statement("0.3.0", "RDM-0.3.0-x64-setup.msi", EMPTY), &good[..12]), "a malformed one");
    }

    #[test]
    fn formats_are_recognised() {
        assert!(format_matches("a.msi", &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0]));
        assert!(format_matches("a.tar.gz", &[0x1F, 0x8B, 8]));
        assert!(format_matches("a.deb", b"!<arch>\ndebian"));
        assert!(!format_matches("a.msi", b"<html>"));
        assert!(!format_matches("a.exe", b"MZ"));
    }

    /// The latest published release, end to end: every package downloads and passes the checks
    /// (GitHub's SHA-256, format, Ed25519 signature). `cargo test -p rdm -- --ignored published`
    #[tokio::test]
    #[ignore = "network: downloads the published release"]
    async fn published_release_packages_verify() {
        let client = reqwest::Client::builder().user_agent("rdm-test").build().unwrap();
        let release = latest(&client).await.unwrap().expect("a published release");
        let version = release.tag_name.trim_start_matches(['v', 'V']).to_owned();
        for method in [Method::Msi, Method::Binary, Method::Deb, Method::Rpm] {
            let package = package(&release.assets, method).unwrap_or_else(|| panic!("{method:?}: no signed package"));
            let file = download(&client, &version, &package, |_| {}).await.unwrap_or_else(|e| panic!("{method:?}: {e}"));
            println!("{method:?}: {} verified", package.name);
            let _ = std::fs::remove_file(file.path);
        }
        // Replayed as another version, the same packages are refused.
        let msi = package(&release.assets, Method::Msi).unwrap();
        assert!(download(&client, "99.0.0", &msi, |_| {}).await.is_err());
    }
}
