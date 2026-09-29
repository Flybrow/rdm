//! Installing a checked package: the Windows installation assistant, the Linux binary or package
//! manager; the update's leftovers.

use super::*;

/// The command-line flag of the installation assistant: `rdm --apply-update <msi> <pid> <exe>`.
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
pub fn start_installation(msi: &Path) -> std::io::Result<()> {
    let target = installed_exe().ok_or(std::io::ErrorKind::Unsupported)?;
    let helper = std::env::temp_dir().join(format!("rdm-updater-{}.exe", std::process::id()));
    std::fs::copy(startup_exe().map_or_else(std::env::current_exe, |p| Ok(p.to_path_buf()))?, &helper)?;
    let mut command = std::process::Command::new(&helper);
    command.arg(HELPER_FLAG).arg(msi).arg(std::process::id().to_string()).arg(&target);
    imp::spawn_outliving(&mut command)
}

/// Linux: installs the downloaded package (binary replaced in place, or the package manager behind
/// the system's password prompt). RDM restarts afterwards.
pub async fn install_linux(package: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || linux::install(method(), &package)).await.map_err(|e| e.to_string())?
}

/// After quitting for an update: starts the (new) RDM from where it was installed.
pub fn relaunch() {
    if let Some(exe) = startup_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

/// In the assistant: `true` when `args` asked for it (it then did its job).
pub fn run_assistant(args: &[String]) -> bool {
    let [flag, msi, pid, exe] = args else { return false };
    if flag != HELPER_FLAG {
        return false;
    }
    imp::apply(Path::new(msi), pid.parse().unwrap_or(0), Path::new(exe));
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
    use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

    use super::{Method, startup_exe, which};
    use crate::{tr, trf};

    pub fn install(method: Method, package: &Path) -> Result<(), String> {
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

    /// The package manager, as root after the system's password prompt (polkit).
    fn elevated(tool: &[&str], package: &Path) -> Result<(), String> {
        let status = Command::new("pkexec").args(tool).arg(package).status().map_err(|e| e.to_string())?;
        match status.code() {
            Some(0) => Ok(()),
            Some(126 | 127) => Err(tr!("mot de passe refusé ou demande fermée", "password refused or prompt closed").into()),
            Some(c) => Err(trf!("le gestionnaire de paquets a échoué (code {c})", "the package manager failed (code {c})", c = c)),
            None => Err(tr!("installation interrompue", "installation interrupted").into()),
        }
    }

    /// Unpacks the tarball and swaps the binary in place (a rename: the running process keeps its
    /// file until it exits, and a crash half-way leaves the old binary intact).
    fn replace_binary(archive: &Path) -> Result<(), String> {
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
    use std::path::Path;

    use super::Method;

    pub fn install(_: Method, _: &Path) -> Result<(), String> {
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

    pub fn apply(msi: &Path, pid: u32, exe: &Path) {
        wait_for_exit(pid);
        // The single-instance lock (the bridge port) is the last thing RDM lets go.
        wait_until(Duration::from_secs(15), || TcpListener::bind((Ipv4Addr::LOCALHOST, BRIDGE_PORT)).is_ok());

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

    pub fn apply(_: &Path, _: u32, _: &Path) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_arguments() {
        assert!(!run_assistant(&["--minimized".into()]));
        assert!(!run_assistant(&["--other".into(), "a".into(), "1".into(), "b".into()]));
    }
}
