//! Installing a checked package: the Windows installation assistant, the Linux binary or package
//! manager; the update's leftovers.

use super::*;

/// The command-line flag of the installation assistant: `rdm --apply-update <msi> <pid> <exe>
/// <sha256>` (the package's SHA-256, checked again just before installing it).
pub const HELPER_FLAG: &str = "--apply-update";

/// Where the Windows installer puts RDM (per user).
pub fn installed_exe() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Programs\RDM\rdm.exe"))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Windows: starts the installation assistant for `msi` (see the module docs). The caller quits
/// right after.
pub fn start_installation(msi: &Verified) -> std::io::Result<()> {
    let target = installed_exe().ok_or(std::io::ErrorKind::Unsupported)?;
    let helper = std::env::temp_dir().join(format!("rdm-updater-{}.exe", std::process::id()));
    std::fs::copy(startup_exe().map_or_else(std::env::current_exe, |p| Ok(p.to_path_buf()))?, &helper)?;
    let mut command = std::process::Command::new(&helper);
    command.arg(HELPER_FLAG).arg(&msi.path).arg(std::process::id().to_string()).arg(&target).arg(&msi.sha256);
    imp::spawn_outliving(&mut command)
}

/// Linux: installs the downloaded package (binary replaced in place, or the package manager behind
/// the system's password prompt). RDM restarts afterwards.
pub async fn install_linux(package: Verified) -> Result<(), String> {
    tokio::task::spawn_blocking(move || linux::install(method(), &package)).await.map_err(|e| e.to_string())?
}

/// Whether the file at `path` is still the package that was verified (its SHA-256 `expected`).
fn unchanged(path: &Path, expected: &str) -> bool {
    crate::manager::checksum::digest(path, crate::manager::checksum::Algo::Sha256).is_ok_and(|digest| digest.eq_ignore_ascii_case(expected))
}

/// After quitting for an update: starts the (new) RDM from where it was installed.
pub fn relaunch() {
    if let Some(exe) = startup_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// In the assistant: `true` when `args` asked for it (it then did its job).
pub fn run_assistant(args: &[String]) -> bool {
    let [flag, msi, pid, exe, sha256] = args else { return false };
    if flag != HELPER_FLAG {
        return false;
    }
    imp::apply(Path::new(msi), pid.parse().unwrap_or(0), Path::new(exe), sha256);
    true
}

/// Where update packages are downloaded: the user's own temporary folder on Windows; on Linux a
/// private folder (`~/.cache/rdm`, 0700) rather than the shared `/tmp`, where another account
/// could prepare a file or a link under the expected name.
pub(super) fn work_dir() -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = directories::BaseDirs::new().ok_or(std::io::ErrorKind::NotFound)?.cache_dir().join("rdm");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }
    #[cfg(not(unix))]
    {
        Ok(std::env::temp_dir())
    }
}

/// At start: leftovers of a previous update (package, assistant copy, old logs). Files still in
/// use (an assistant finishing its job) are left for next time.
pub fn clean_leftovers() {
    let Ok(dir) = work_dir().and_then(std::fs::read_dir) else { return };
    let week = Duration::from_secs(7 * 24 * 3600);
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = || entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > week));
        let update = name.starts_with("rdm-update-");
        let leftover = name.starts_with("rdm-updater-") && name.ends_with(".exe")
            || update && [".msi", ".tmp", ".tar.gz", ".deb", ".rpm"].iter().any(|e| name.ends_with(e))
            || update && name.ends_with(".log") && old();
        if leftover {
            let _ = std::fs::remove_file(entry.path());
        } else if update && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path()); // an unpacked tarball (Linux)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{os::unix::fs::PermissionsExt, process::Command};

    use super::{Method, Verified, startup_exe, unchanged, which};
    use crate::{tr, trf};

    pub fn install(method: Method, package: &Verified) -> Result<(), String> {
        match method {
            Method::Binary => replace_binary(package),
            Method::Deb => {
                let tool = if which("apt-get").is_some() { vec!["apt-get", "install", "-y", "--allow-downgrades"] } else { vec!["dpkg", "-i"] };
                elevated(&tool, package)
            }
            Method::Rpm => {
                let tool = if which("dnf").is_some() {
                    vec!["dnf", "install", "-y"]
                } else if which("zypper").is_some() {
                    vec!["zypper", "--non-interactive", "install", "--allow-unsigned-rpm"]
                } else {
                    vec!["rpm", "-U", "--replacepkgs"]
                };
                elevated(&tool, package)
            }
            Method::Msi | Method::Manual => Err(tr!("mise à jour impossible ici", "cannot update this copy").into()),
        }
    }

    /// Run as root (`sh -c` with the package, its verified SHA-256, then the package manager's
    /// command): the package is copied where only root can write, checked to be the very file RDM
    /// verified, and that copy is installed. Another program of the account cannot swap the
    /// package between RDM's checks and the installation as root. Exit code 3: it was swapped.
    pub(super) const AS_ROOT: &str = r#"set -eu
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
copy="$dir/$(basename "$1")"
cp -- "$1" "$copy"
chmod 755 "$dir"
printf '%s  %s\n' "$2" "$copy" | sha256sum -c --status - || exit 3
shift 2
"$@" "$copy"
"#;

    /// The package manager, as root after the system's password prompt (polkit).
    fn elevated(tool: &[&str], package: &Verified) -> Result<(), String> {
        let status = Command::new("pkexec")
            .args(["/bin/sh", "-c", AS_ROOT, "sh"])
            .arg(&package.path)
            .arg(&package.sha256)
            .args(tool)
            .status()
            .map_err(|e| e.to_string())?;
        match status.code() {
            Some(0) => Ok(()),
            Some(3) => Err(tr!("le paquet a été modifié après sa vérification : installation refusée", "the package changed after it was verified: installation refused").into()),
            Some(126 | 127) => Err(tr!("mot de passe refusé ou demande fermée", "password refused or prompt closed").into()),
            Some(c) => Err(trf!("le gestionnaire de paquets a échoué (code {c})", "the package manager failed (code {c})", c = c)),
            None => Err(tr!("installation interrompue", "installation interrupted").into()),
        }
    }

    /// Unpacks the tarball and swaps the binary in place (a rename: the running process keeps its
    /// file until it exits, and a crash half-way leaves the old binary intact).
    fn replace_binary(package: &Verified) -> Result<(), String> {
        // Everything here runs as the user: checked again all the same, just before use.
        if !unchanged(&package.path, &package.sha256) {
            return Err(tr!("le paquet a été modifié après sa vérification : installation refusée", "the package changed after it was verified: installation refused").into());
        }
        let archive = package.path.as_path();
        let exe = startup_exe().ok_or_else(|| tr!("emplacement de RDM inconnu", "RDM's location is unknown").to_owned())?;
        let work = super::work_dir().map_err(|e| e.to_string())?.join(format!("rdm-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let result = (|| {
            let ok = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&work).status().is_ok_and(|s| s.success());
            let new = work.join("rdm-linux-x64").join("rdm");
            let elf = std::fs::read(&new).is_ok_and(|b| b.starts_with(b"\x7fELF"));
            if !ok || !elf {
                return Err(tr!("archive de mise à jour illisible", "unreadable update archive").to_owned());
            }
            let staged = exe.with_file_name(".rdm-update");
            std::fs::copy(&new, &staged).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            std::fs::rename(&staged, exe).map_err(|e| e.to_string())
        })();
        let _ = std::fs::remove_dir_all(&work);
        result
    }
}

#[cfg(not(target_os = "linux"))]
mod linux {
    use super::{Method, Verified};

    pub fn install(_: Method, _: &Verified) -> Result<(), String> {
        Err(crate::tr!("mise à jour impossible ici", "cannot update this copy").into())
    }
}

#[cfg(windows)]
mod imp {
    use std::{
        net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
        os::windows::process::CommandExt,
        path::Path,
        process::Command,
        thread::sleep,
        time::{Duration, Instant},
    };

    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject},
        UI::WindowsAndMessaging::{IDYES, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, MessageBoxW},
    };

    use crate::{settings::BRIDGE_PORT, tr, trf};

    /// Outside a job that would end it together with RDM (when the job allows it).
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    /// How long RDM gets to close cleanly (downloads saved) before the assistant ends it.
    const CLOSE_GRACE: Duration = Duration::from_secs(30);
    /// Another installation running (Windows Update…): the installer is retried for this long.
    const BUSY_RETRIES: u32 = 30;

    pub fn spawn_outliving(command: &mut Command) -> std::io::Result<()> {
        match command.creation_flags(CREATE_BREAKAWAY_FROM_JOB).spawn() {
            Ok(_) => Ok(()),
            Err(_) => command.creation_flags(0).spawn().map(drop),
        }
    }

    pub fn apply(msi: &Path, pid: u32, exe: &Path, sha256: &str) {
        wait_for_exit(pid);
        // The single-instance lock (the bridge port) is the last thing RDM lets go.
        wait_until(Duration::from_secs(15), || TcpListener::bind((Ipv4Addr::LOCALHOST, BRIDGE_PORT)).is_ok());

        // Still the package RDM verified (its signature covers this SHA-256)? Never install another.
        if !super::unchanged(msi, sha256) {
            crate::notify::fatal(tr!(
                "La mise à jour de RDM a été modifiée après sa vérification : elle n'est pas installée. RDM redémarre dans sa version actuelle.",
                "RDM's update changed after it was verified: it is not installed. RDM restarts in its current version."
            ));
            let _ = Command::new(exe).spawn();
            return;
        }
        let log = msi.with_extension("log");
        // Silent: no window at all; RDM's own card said "installing", and RDM comes back.
        let code = msiexec(&["/i".as_ref(), msi.as_os_str(), "/qn".as_ref(), "/norestart".as_ref(), "/l*v".as_ref(), log.as_os_str()]);
        // 3010 / 1641: installed, a restart completes it.
        if !matches!(code, Some(0 | 3010 | 1641)) {
            let reason = match code {
                Some(1602) => tr!("installation annulée", "installation cancelled").to_owned(),
                Some(1603) => tr!("erreur de Windows Installer (1603)", "Windows Installer error (1603)").to_owned(),
                Some(1618) => tr!("une autre installation est en cours (1618)", "another installation is in progress (1618)").to_owned(),
                Some(c) => trf!("code {c}", "code {c}", c = c),
                None => tr!("Windows Installer n'a pas pu être lancé", "Windows Installer could not be started").to_owned(),
            };
            let log = log.display();
            let text = trf!(
                "La mise à jour de RDM n'a pas pu s'installer : {reason}.\n\nOuvrir l'installateur pour réessayer ? (Sinon, RDM redémarre dans sa version actuelle.)\n\nJournal : {log}",
                "RDM's update could not be installed: {reason}.\n\nOpen the installer to try again? (Otherwise RDM restarts in its current version.)\n\nLog: {log}",
                log = log,
                reason = reason
            );
            if ask(&text) {
                msiexec(&["/i".as_ref(), msi.as_os_str()]);
            }
        }
        // The installer starts RDM itself when it succeeds; if it did not (or it failed), here.
        if !wait_until(Duration::from_secs(12), rdm_running) {
            let _ = Command::new(exe).spawn();
        }
    }

    /// Waits for RDM (`pid`) to exit; ends it if it does not within `CLOSE_GRACE`.
    fn wait_for_exit(pid: u32) {
        // SAFETY: plain Win32 calls on a handle we own and close.
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return; // already gone
            }
            if WaitForSingleObject(process, CLOSE_GRACE.as_millis() as u32) == WAIT_TIMEOUT {
                TerminateProcess(process, 1);
                WaitForSingleObject(process, 10_000);
            }
            CloseHandle(process);
        }
    }

    /// `msiexec` with `args`, waited for; retried while another installation is running. `None`
    /// when it could not be started.
    fn msiexec(args: &[&std::ffi::OsStr]) -> Option<i32> {
        let exe = std::env::var_os("SystemRoot").map_or_else(|| "msiexec.exe".into(), |root| Path::new(&root).join(r"System32\msiexec.exe"));
        for _ in 0..BUSY_RETRIES {
            let code = Command::new(&exe).args(args).status().ok()?.code();
            if code != Some(1618) {
                return code;
            }
            sleep(Duration::from_secs(10));
        }
        Some(1618)
    }

    fn rdm_running() -> bool {
        TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, BRIDGE_PORT)), Duration::from_millis(300)).is_ok()
    }

    fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if done() {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            sleep(Duration::from_millis(250));
        }
    }

    fn ask(text: &str) -> bool {
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide(tr!("RDM — mise à jour", "RDM — update")));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; no owner window.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND) == IDYES }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::{path::Path, process::Command};

    pub fn spawn_outliving(_: &mut Command) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    pub fn apply(_: &Path, _: u32, _: &Path, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_arguments() {
        assert!(!run_assistant(&["--minimized".into()]));
        assert!(!run_assistant(&["--other".into(), "a".into(), "1".into(), "b".into(), "c".into()]));
        assert!(!run_assistant(&[HELPER_FLAG.into(), "a.msi".into(), "1".into(), "rdm.exe".into()]), "the SHA-256 is required");
    }

    /// The package is checked again at the last moment against the SHA-256 its signature covers.
    #[test]
    fn a_package_changed_after_its_check_is_noticed() {
        let path = std::env::temp_dir().join(format!("rdm-unchanged-{}.bin", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(unchanged(&path, EMPTY) && unchanged(&path, &EMPTY.to_ascii_uppercase()));
        std::fs::write(&path, b"swapped").unwrap();
        assert!(!unchanged(&path, EMPTY));
        let _ = std::fs::remove_file(&path);
        assert!(!unchanged(&path, EMPTY), "gone");
    }

    /// What `pkexec` runs as root: the copy it installs is the verified file, or nothing is.
    #[cfg(target_os = "linux")]
    #[test]
    fn root_installs_only_the_verified_copy() {
        let path = std::env::temp_dir().join(format!("rdm_{}_amd64.deb", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let run = |sha: &str, tool: &[&str]| {
            std::process::Command::new("sh").args(["-c", linux::AS_ROOT, "sh"]).arg(&path).arg(sha).args(tool).status().unwrap().code()
        };
        // The tool gets a copy of the same name, in a fresh folder: not the user's file ($1 here).
        let original = path.to_str().unwrap();
        let check = r#"test -f "$2" && test "$2" != "$1" && test "$(basename "$2")" = "$(basename "$1")""#;
        assert_eq!(run(EMPTY, &["sh", "-c", check, "sh", original]), Some(0));
        assert_eq!(run(&"0".repeat(64), &["true"]), Some(3), "swapped: never installed");
        let _ = std::fs::remove_file(&path);
    }
}
