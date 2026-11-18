//! Keeping idle clients out of physical memory.
//!
//! Windows keeps every page a process has touched in its working set. A client
//! that sits in the background with a 1.5 GB working set pushes six-window setups
//! into the pagefile. `EmptyWorkingSet` moves those pages to the standby list: the
//! process keeps running, the memory goes back to the system, and the pages are
//! read in again on demand.
//!
//! The foreground client is skipped on purpose, so the window in use never pays
//! for a refault. Nothing is injected and nothing is killed: this is a plain
//! kernel call on processes we started.

use std::sync::Once;
use std::time::Duration;

use crate::processes;

/// How often background clients are trimmed. Often enough to matter, rare
/// enough not to fight a client that is loading a place.
const INTERVAL: Duration = Duration::from_secs(20);

static STARTED: Once = Once::new();

/// Start the trimmer. Safe to call many times: only the first call spawns it.
pub fn enable() {
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("rbm-memory".into())
            .spawn(|| loop {
                std::thread::sleep(INTERVAL);
                trim_background();
            });
        // A failed thread only means no trimming.
        let _ = spawned;
    });
}

/// Give the working set of every background client back to the system.
/// Returns how many processes were trimmed.
pub fn trim_background() -> usize {
    let foreground = processes::foreground_pid();
    processes::client_pids()
        .into_iter()
        .filter(|pid| Some(*pid) != foreground)
        .filter(|pid| win::empty_working_set(*pid))
        .count()
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "psapi")]
    extern "system" {
        fn EmptyWorkingSet(process: Handle) -> i32;
    }

    /// Move the pages of `pid` to the standby list. The process keeps running.
    pub(super) fn empty_working_set(pid: u32) -> bool {
        // SAFETY: the handle is closed before returning.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_QUERY_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let ok = unsafe { EmptyWorkingSet(process) };
        unsafe { CloseHandle(process) };
        ok != 0
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn empty_working_set(_pid: u32) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_without_clients_is_harmless() {
        // Whatever is running here, the call must finish without touching the
        // foreground process and without panicking.
        let _ = trim_background();
    }
}
