//! Keeping idle clients out of physical memory.
//!
//! Windows keeps every page a process has touched in its working set. A client
//! that sits in the background with a 1.5 GB working set pushes six-window setups
//! into the pagefile. `EmptyWorkingSet` moves those pages to the standby list: the
//! process keeps running, the memory goes back to the system, and the pages are
//! read in again on demand.
//!
//! On Linux the client is Sober, one Flatpak instance per account, and every
//! instance lives in its own systemd scope with a cgroup v2 memory controller.
//! Writing `memory.reclaim` asks the kernel to reclaim that many bytes from the
//! whole instance: page cache, anonymous pages that can be swapped, and the
//! engine's unused allocations. The process is not frozen, not injected into and
//! not killed; it faults its pages back in when it needs them.
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

/// A reclaim smaller than this is not worth the wakeup.
const RECLAIM_FLOOR: u64 = 256 << 20;
/// Never ask for more than this in one go; the kernel takes it from there.
const RECLAIM_MAX: u64 = 512 << 20;

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

/// Whether this system has a memory controller the manager can write to.
#[cfg(windows)]
pub fn supported() -> bool {
    true
}

/// Linux: cgroup v2 with the memory controller is what makes `memory.reclaim`
/// exist at all.
#[cfg(target_os = "linux")]
pub fn supported() -> bool {
    std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers")
        .map(|text| text.split_whitespace().any(|name| name == "memory"))
        .unwrap_or(false)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn supported() -> bool {
    false
}

/// Give the memory of every background client back to the system.
/// Returns how many instances were trimmed.
pub fn trim_background() -> usize {
    #[cfg(windows)]
    {
        let foreground = processes::foreground_pid();
        processes::client_pids()
            .into_iter()
            .filter(|pid| Some(*pid) != foreground)
            .filter(|pid| win::empty_working_set(*pid))
            .count()
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland gives no foreground pid; KWin knows which window is active.
        // Map it back to a cgroup, because one instance is several processes.
        let active =
            crate::kwin::active_window().and_then(|window| processes::cgroup_path(window.pid));
        processes::client_pids()
            .into_iter()
            .filter_map(processes::cgroup_path)
            .filter(|cgroup| active.as_deref() != Some(cgroup.as_str()))
            .filter(|cgroup| win::reclaim(cgroup))
            .count()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        0
    }
}

/// How much to ask the kernel to reclaim from an instance using `current`
/// bytes. `None` when the instance is small enough to leave alone.
///
/// An eighth of the footprint is enough to matter for six clients and small
/// enough not to empty a game that is about to be used; the clamp keeps a tiny
/// client from a useless wakeup and a huge one from a multi-second stall.
fn reclaim_budget(current: u64) -> Option<u64> {
    if current < RECLAIM_FLOOR {
        return None;
    }
    Some((current / 8).clamp(32 << 20, RECLAIM_MAX))
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

#[cfg(target_os = "linux")]
mod win {
    use std::fs;
    use std::path::Path;

    /// Reclaim memory from the cgroup of one Sober instance.
    pub(super) fn reclaim(cgroup: &str) -> bool {
        let dir = Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'));
        let Ok(text) = fs::read_to_string(dir.join("memory.current")) else {
            return false;
        };
        let Ok(current) = text.trim().parse::<u64>() else {
            return false;
        };
        let Some(amount) = super::reclaim_budget(current) else {
            return false;
        };
        // `memory.reclaim` takes bytes. A failed write only means this instance
        // keeps its pages; the next round tries again.
        fs::write(dir.join("memory.reclaim"), amount.to_string()).is_ok()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod win {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_instance_is_left_alone() {
        assert_eq!(reclaim_budget(0), None);
        assert_eq!(reclaim_budget(RECLAIM_FLOOR - 1), None);
        assert_eq!(reclaim_budget(RECLAIM_FLOOR), Some(32 << 20));
    }

    #[test]
    fn the_reclaim_grows_with_the_instance_and_stops_at_the_cap() {
        assert_eq!(reclaim_budget(1 << 30), Some(128 << 20));
        assert_eq!(reclaim_budget(8 << 30), Some(RECLAIM_MAX));
        assert_eq!(reclaim_budget(u64::MAX / 2), Some(RECLAIM_MAX));
    }

    #[test]
    fn the_memory_controller_is_looked_for_not_assumed() {
        // Whatever this machine has, the check must be cheap and safe.
        let _ = supported();
    }

    #[test]
    #[ignore = "reclaims memory from live Sober instances"]
    fn smoke_reclaim() {
        println!("active: {:?}", crate::kwin::active_window());
        let instances: Vec<(u32, Option<String>)> = processes::client_pids()
            .into_iter()
            .map(|pid| (pid, processes::cgroup_path(pid)))
            .collect();
        println!("instances: {instances:?}");
        let trimmed = trim_background();
        println!("reclaimed from {trimmed} instances");
    }
}
