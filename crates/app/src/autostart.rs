//! Launch at login (minimized): `HKCU\…\Run` on Windows, XDG autostart entry on Linux.

pub const MINIMIZED_FLAG: &str = "--minimized";

pub fn set(enabled: bool) -> std::io::Result<()> {
    imp::set(enabled, &std::env::current_exe()?)
}

#[cfg(windows)]
mod imp {
    use std::{io, path::Path};

    use winreg::{RegKey, enums::HKEY_CURRENT_USER};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn set(enabled: bool, exe: &Path) -> io::Result<()> {
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN)?;
        if enabled {
            run.set_value("RDM", &format!("\"{}\" {}", exe.display(), super::MINIMIZED_FLAG))
        } else {
            match run.delete_value("RDM") {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::{fs, io, path::Path};

    pub fn set(enabled: bool, exe: &Path) -> io::Result<()> {
        let dir = directories::BaseDirs::new().ok_or(io::ErrorKind::NotFound)?.config_dir().join("autostart");
        let entry = dir.join("rdm.desktop");
        if !enabled {
            return match fs::remove_file(entry) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        fs::create_dir_all(&dir)?;
        fs::write(
            entry,
            format!(
                "[Desktop Entry]\nType=Application\nName=RDM\nExec=\"{}\" {}\nIcon=rdm\nX-GNOME-Autostart-enabled=true\n",
                super::desktop_quote(&exe.to_string_lossy()),
                super::MINIMIZED_FLAG
            ),
        )
    }
}

/// Escapes a path for a quoted `Exec=` argument (Desktop Entry spec): `"` `` ` `` `$` `\` get a
/// backslash (itself doubled by the string-value escaping), and field codes' `%` is doubled.
#[cfg_attr(windows, allow(dead_code))]
fn desktop_quote(path: &str) -> String {
    path.chars().fold(String::with_capacity(path.len()), |mut out, c| {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
        out
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn desktop_exec_escaping() {
        assert_eq!(super::desktop_quote("/opt/r d m/rdm"), "/opt/r d m/rdm");
        assert_eq!(super::desktop_quote("/a\"$b%/rdm"), "/a\\\\\"\\\\$b%%/rdm");
    }
}
