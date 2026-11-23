//! Start with the session.
//!
//! Windows: one value under the per-user `Run` key, changed with the `reg` tool
//! that ships with the system, so no administrator rights are needed.
//!
//! Linux: the XDG autostart directory, which every desktop reads —
//! `~/.config/autostart/RbxAccountManager.desktop`.

use std::path::Path;

/// What goes into a launcher: the executable path, quoted so that a folder with
/// spaces in its name still works.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(windows)]
mod system {
    use std::process::Command;

    use super::command_for;
    use crate::roblox::run_hidden;

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "RbxAccountManager";

    pub(super) fn is_enabled() -> bool {
        run_hidden(Command::new("reg").args(["query", RUN_KEY, "/v", VALUE_NAME])).is_some()
    }

    pub(super) fn set_enabled(enabled: bool) -> Result<(), String> {
        let done = if enabled {
            let exe = std::env::current_exe()
                .map_err(|err| format!("Не нашёл путь к программе: {err}"))?;
            run_hidden(Command::new("reg").args([
                "add",
                RUN_KEY,
                "/v",
                VALUE_NAME,
                "/t",
                "REG_SZ",
                "/d",
                &command_for(&exe),
                "/f",
            ]))
            .is_some()
        } else if is_enabled() {
            run_hidden(Command::new("reg").args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"]))
                .is_some()
        } else {
            // Nothing to remove.
            true
        };

        if done {
            Ok(())
        } else {
            Err("Не удалось изменить автозапуск (реестр отказал)".into())
        }
    }
}

#[cfg(not(windows))]
mod system {
    use std::fs;
    use std::path::PathBuf;

    use super::command_for;

    /// `~/.config/autostart/RbxAccountManager.desktop`
    fn entry_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("autostart").join("RbxAccountManager.desktop"))
    }

    pub(super) fn is_enabled() -> bool {
        entry_path().is_some_and(|path| path.is_file())
    }

    pub(super) fn set_enabled(enabled: bool) -> Result<(), String> {
        let path = entry_path().ok_or("Не нашёл домашний каталог")?;
        if !enabled {
            return match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(format!("не могу удалить автозапуск: {err}")),
            };
        }

        let exe =
            std::env::current_exe().map_err(|err| format!("Не нашёл путь к программе: {err}"))?;
        let body = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=RbxAccountManager\n\
             Comment=Менеджер аккаунтов Roblox\n\
             Exec={}\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n\
             X-KDE-autostart-after=panel\n",
            command_for(&exe)
        );
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("не могу создать {}: {err}", parent.display()))?;
        }
        fs::write(&path, body).map_err(|err| format!("не могу записать {}: {err}", path.display()))
    }
}

/// Whether the manager is registered to start with the session.
pub fn is_enabled() -> bool {
    system::is_enabled()
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    system::set_enabled(enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_quoted() {
        assert_eq!(
            command_for(Path::new(r"C:\Program Files\rbm\rbm.exe")),
            r#""C:\Program Files\rbm\rbm.exe""#
        );
        assert_eq!(command_for(Path::new("/home/me/rbm")), r#""/home/me/rbm""#);
    }

    #[test]
    fn off_windows_the_registry_is_not_used() {
        if !cfg!(windows) {
            // The XDG entry must never appear just because it was asked about.
            let was = is_enabled();
            assert!(!Path::new(r"HKCU\Software").exists());
            assert_eq!(is_enabled(), was);
        }
    }
}
