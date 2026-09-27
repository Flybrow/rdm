//! Desktop notifications (Windows toast / freedesktop D-Bus): a download completes, a VirusTotal
//! verdict arrives.

use crate::virustotal::Report;

#[cfg(windows)]
const APP_ID: &str = "RDM.DownloadManager";

/// Windows attributes toasts to a registered AppUserModelID: without it they would appear to come
/// from "Windows PowerShell". Per-user registry only; removed by the uninstaller.
pub fn register() {
    #[cfg(windows)]
    {
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};

        const ICON: &[u8] = include_bytes!("../assets/rdm.png");
        let icon = crate::settings::config_file("rdm.png");
        // Rewritten when the bundled icon changed (a new version), not on every start.
        if std::fs::read(&icon).ok().as_deref() != Some(ICON) {
            if let Some(dir) = icon.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&icon, ICON);
        }
        let path = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
        if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(path) {
            let _ = key.set_value("DisplayName", &"RDM");
            let _ = key.set_value("IconUri", &icon.to_string_lossy().into_owned());
        }
    }
}

/// At most one every few seconds: finishing a batch of small files must not spam the desktop.
pub fn completed(file_name: &str) {
    use std::{
        sync::atomic::{AtomicU64, Ordering::Relaxed},
        time::{SystemTime, UNIX_EPOCH},
    };
    static LAST: AtomicU64 = AtomicU64::new(0);
    const QUIET_SECS: u64 = 3;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if now.saturating_sub(LAST.swap(now, Relaxed)) < QUIET_SECS {
        return;
    }
    show("Téléchargement terminé", file_name);
}

pub fn virustotal(file_name: &str, verdict: Result<&Report, String>) {
    let summary = match &verdict {
        Ok(r) if r.flagged() == 0 => format!("VirusTotal : aucune menace ({}/{})", r.flagged(), r.engines()),
        Ok(r) => format!("VirusTotal : {} détection(s) sur {}", r.flagged(), r.engines()),
        Err(_) => "VirusTotal : analyse impossible".to_owned(),
    };
    let body = match verdict {
        Err(reason) => format!("{file_name} — {reason}"),
        Ok(_) => file_name.to_owned(),
    };
    show(&summary, &body);
}

/// RDM cannot run: said in a message box (Windows) or a notification (Linux), waited for — the
/// process ends right after.
pub fn fatal(text: &str) {
    eprintln!("RDM: {text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide("RDM"));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; no owner window.
        unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR | MB_SETFOREGROUND) };
    }
    #[cfg(not(windows))]
    {
        let _ = notify_rust::Notification::new().appname("RDM").summary("RDM").body(&escape_markup(text)).icon("rdm").show();
    }
}

/// Fire-and-forget: showing a notification may block on D-Bus or WinRT for a moment.
fn show(summary: &str, body: &str) {
    // Linux notification servers may interpret the body as markup.
    let body = if cfg!(windows) { body.to_owned() } else { escape_markup(body) };
    let summary = summary.to_owned();
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.appname("RDM").summary(&summary).body(&body);
        #[cfg(windows)]
        n.app_id(APP_ID);
        #[cfg(not(windows))]
        n.icon("rdm");
        let _ = n.show();
    });
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    #[test]
    fn markup_is_escaped() {
        assert_eq!(super::escape_markup("<b>a&b</b>.mp4"), "&lt;b&gt;a&amp;b&lt;/b&gt;.mp4");
    }
}
