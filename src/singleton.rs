//! Several Roblox clients at once.
//!
//! The client guards itself with named objects: `ROBLOX_singletonEvent` and, in
//! some builds, `ROBLOX_singletonMutex`. A client that finds either name already
//! taken signals the running instance to quit and takes its place — that is why
//! a newly launched window closed the previous one even with `--app`.
//!
//! Creating a **mutex** with the same name in our own process makes the client's
//! `CreateEventW` fail. The client logs "cannot create event to secure single
//! process" and keeps running without the guard, so every later client also
//! starts freely. Both names are claimed for the whole run of the manager, which
//! means the first client has to be started after it.
//!
//! Nothing is injected into the client and no files are patched: the client just
//! finds the name busy, exactly as if another manager held it.

use std::sync::Mutex;

/// Names the client uses for its single-instance guard.
const NAMES: [&str; 2] = ["ROBLOX_singletonEvent", "ROBLOX_singletonMutex"];

/// Handles of the objects we hold. They stay open for the whole run.
static HELD: Mutex<Option<Vec<usize>>> = Mutex::new(None);

/// Claim the guard names. Safe to call again: after the first success this is a
/// no-op, after a failure it retries (the client that held a name may be gone).
pub fn enable() -> Result<(), String> {
    let mut held = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if held.is_some() {
        return Ok(());
    }

    let mut objects = Vec::with_capacity(NAMES.len());
    for name in NAMES {
        match win::claim_mutex(name) {
            Ok(handle) => objects.push(handle),
            Err(code) => {
                // A half-claimed guard protects nothing, so release it and let
                // the next attempt start from a clean slate.
                for handle in objects.drain(..) {
                    win::close(handle);
                }
                return Err(message(name, code));
            }
        }
    }

    *held = Some(objects);
    Ok(())
}

/// What to tell the user when a name could not be claimed.
fn message(name: &str, code: u32) -> String {
    if code == win::ERROR_INVALID_HANDLE {
        format!(
            "Несколько окон не включились: имя {name} уже занято запущенным Roblox. \
             Закрой все окна Roblox и запусти игру из менеджера снова"
        )
    } else {
        format!(
            "Несколько окон не включились: не удалось занять {name} \
             (Windows error {code}). Перезапусти менеджер"
        )
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::ptr;

    type Handle = *mut c_void;

    /// The name belongs to an object of another type, i.e. the real event.
    pub const ERROR_INVALID_HANDLE: u32 = 6;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attributes: *const c_void, owner: i32, name: *const u16) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    /// Create (or open) a mutex with this name.
    ///
    /// A handle is returned even when the mutex already exists, because the name
    /// stays reserved either way. Failure means the name is taken by another
    /// kind of object — the client's own event — and nothing was reserved.
    pub fn claim_mutex(name: &str) -> Result<usize, u32> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // Owner is left false on purpose: we only need the name to exist.
        let handle = unsafe { CreateMutexW(ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(unsafe { GetLastError() });
        }
        Ok(handle as usize)
    }

    pub fn close(handle: usize) {
        unsafe { CloseHandle(handle as Handle) };
    }
}

#[cfg(not(windows))]
mod win {
    pub const ERROR_INVALID_HANDLE: u32 = 6;

    pub fn claim_mutex(_name: &str) -> Result<usize, u32> {
        Ok(0)
    }

    pub fn close(_handle: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_calls_agree() {
        // A client started before the tests owns the names; then both calls
        // fail the same way and nothing is claimed.
        let first = enable();
        let second = enable();
        assert_eq!(first.is_ok(), second.is_ok());
    }

    #[test]
    fn busy_name_points_at_a_running_client() {
        let text = message("ROBLOX_singletonEvent", win::ERROR_INVALID_HANDLE);
        assert!(text.contains("уже занято запущенным Roblox"));
        assert!(text.contains("Закрой все окна Roblox"));
    }

    #[test]
    fn other_errors_show_the_code() {
        let text = message("ROBLOX_singletonMutex", 5);
        assert!(text.contains("Windows error 5"));
    }
}
