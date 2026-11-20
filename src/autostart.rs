//! Start with Windows: one value under the per-user `Run` key.
//!
//! Per-user (`HKCU`), so no administrator rights are needed and other Windows
//! users are not affected. The key is changed with the `reg` tool that ships with
//! Windows, the same way the client path is read in `roblox.rs`.

use std::path::Path;
use std::process::Command;

use crate::roblox::run_hidden;

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "RbxAccountManager";

/// What goes into the registry value: the executable path, quoted so that a
/// folder with spaces in its name still works.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// Whether the manager is registered to start with Windows.
pub fn is_enabled() -> bool {
    run_hidden(Command::new("reg").args(["query", RUN_KEY, "/v", VALUE_NAME])).is_some()
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Автозапуск доступен только в Windows".into());
    }

    let done = if enabled {
        let exe =
            std::env::current_exe().map_err(|err| format!("Не нашёл путь к программе: {err}"))?;
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
        run_hidden(Command::new("reg").args(["delete", RUN_KEY, "/v", VALUE_NAME, "/f"])).is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_quoted() {
        assert_eq!(
            command_for(Path::new(r"C:\Program Files\rbm\rbm.exe")),
            r#""C:\Program Files\rbm\rbm.exe""#
        );
    }

    #[test]
    fn off_windows_it_refuses_instead_of_pretending() {
        if !cfg!(windows) {
            assert!(set_enabled(true).is_err());
            assert!(!is_enabled());
        }
    }
}
