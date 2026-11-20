//! Which Roblox processes are running, and which one is in front.
//!
//! The client can move its game window into a process started after the one we
//! spawned, so the manager has to look at processes, not at a single pid. Both
//! the window pinner and the memory trimmer need this list.

/// Executable name of the game client.
#[cfg_attr(not(windows), allow(dead_code))]
const CLIENT_EXE: &str = "RobloxPlayerBeta.exe";

/// Ids of running client processes.
pub fn client_pids() -> Vec<u32> {
    win::client_pids(0)
}

/// Ids of client processes started at or after `since`, a FILETIME tick count.
pub fn client_pids_started_after(since: u64) -> Vec<u32> {
    win::client_pids(since)
}

/// Current time as FILETIME ticks (100 ns since 1601), comparable with process
/// creation times.
pub fn now_filetime() -> u64 {
    win::now_filetime()
}

/// Pid that owns the foreground window, if any.
pub fn foreground_pid() -> Option<u32> {
    win::foreground_pid()
}

/// A running client process and when it was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    /// FILETIME ticks, comparable with [`now_filetime`].
    pub created: u64,
}

/// Running client processes with their creation times. A process whose time
/// cannot be read (it just exited, or access was refused) is left out.
pub fn client_procs() -> Vec<Proc> {
    win::client_procs()
}

/// End client processes. Only pids that are client processes *right now* are
/// touched, so a pid that Windows has meanwhile given to something else is safe.
/// Returns how many were ended.
pub fn kill(pids: &[u32]) -> usize {
    let running = client_pids();
    pids.iter()
        .filter(|pid| running.contains(pid))
        .filter(|pid| {
            crate::roblox::run_hidden(std::process::Command::new("taskkill").args([
                "/F",
                "/PID",
                &pid.to_string(),
            ]))
            .is_some()
        })
        .count()
}

/// End every client window. Returns how many were ended.
pub fn kill_all() -> usize {
    kill(&client_pids())
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::time::{SystemTime, UNIX_EPOCH};

    type Handle = *mut c_void;
    type Hwnd = *mut c_void;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    impl FileTime {
        /// FILETIME counts 100 ns ticks since 1601-01-01.
        fn ticks(&self) -> u64 {
            ((self.high as u64) << 32) | self.low as u64
        }
    }

    /// `PROCESSENTRY32W`: enough of the layout to read the name and the id.
    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        pri_class_base: i32,
        flags: u32,
        exe_file: [u16; 260],
    }

    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INVALID_HANDLE_VALUE: isize = -1;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        fn GetProcessTimes(
            process: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> Hwnd;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    }

    /// Every client process, in snapshot order.
    fn all_client_pids() -> Vec<u32> {
        let mut pids = Vec::new();
        // SAFETY: the snapshot handle is closed before returning.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot.is_null() || snapshot as isize == INVALID_HANDLE_VALUE {
            return pids;
        }

        // SAFETY: `ProcessEntry32W` is plain data, so all-zero is valid, and
        // `size` is set to what the API expects.
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;

        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) };
        while more != 0 {
            if is_client(&entry) {
                pids.push(entry.process_id);
            }
            more = unsafe { Process32NextW(snapshot, &mut entry) };
        }

        unsafe { CloseHandle(snapshot) };
        pids
    }

    /// Client processes; only those created at or after `since` when it is set.
    pub(super) fn client_pids(since: u64) -> Vec<u32> {
        let all = all_client_pids();
        if since == 0 {
            return all;
        }
        all.into_iter()
            .filter(|pid| creation_time(*pid).is_some_and(|created| created >= since))
            .collect()
    }

    pub(super) fn client_procs() -> Vec<super::Proc> {
        all_client_pids()
            .into_iter()
            .filter_map(|pid| creation_time(pid).map(|created| super::Proc { pid, created }))
            .collect()
    }

    fn is_client(entry: &ProcessEntry32W) -> bool {
        let len = entry
            .exe_file
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.exe_file.len());
        let name = String::from_utf16_lossy(&entry.exe_file[..len]);
        name.eq_ignore_ascii_case(super::CLIENT_EXE)
    }

    /// FILETIME ticks at which the process was created, if it can be read.
    fn creation_time(pid: u32) -> Option<u64> {
        // SAFETY: the handle is closed before returning.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        let mut creation = FileTime { low: 0, high: 0 };
        let mut exit = FileTime { low: 0, high: 0 };
        let mut kernel = FileTime { low: 0, high: 0 };
        let mut user = FileTime { low: 0, high: 0 };
        let ok =
            unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe { CloseHandle(process) };
        (ok != 0).then(|| creation.ticks())
    }

    pub(super) fn now_filetime() -> u64 {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        since.as_secs() * 10_000_000
            + u64::from(since.subsec_nanos()) / 100
            + 116_444_736_000_000_000
    }

    pub(super) fn foreground_pid() -> Option<u32> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return None;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        (pid != 0).then_some(pid)
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn client_pids(_since: u64) -> Vec<u32> {
        Vec::new()
    }

    pub(super) fn client_procs() -> Vec<super::Proc> {
        Vec::new()
    }

    pub(super) fn now_filetime() -> u64 {
        0
    }

    pub(super) fn foreground_pid() -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_now_is_plausible() {
        // 2020-01-01 as FILETIME ticks; anything sane is far above it.
        let y2020 = 132_223_104_000_000_000u64;
        if cfg!(windows) {
            assert!(now_filetime() > y2020);
        }
    }

    #[test]
    fn scan_returns_real_ids() {
        // Whether or not the client runs here, the scan must finish and return
        // real ids instead of garbage from a wrong struct layout.
        assert!(client_pids().iter().all(|pid| *pid > 4));
    }

    #[test]
    fn procs_carry_plausible_creation_times() {
        let now = now_filetime();
        for proc in client_procs() {
            assert!(proc.pid > 4);
            // Created in the past, not in some garbage-filled future.
            assert!(proc.created <= now);
        }
    }

    #[test]
    fn killing_a_pid_that_is_not_a_client_does_nothing() {
        // Pid 4 is the Windows kernel, pid 1 is init elsewhere: neither is a
        // Roblox client, so nothing may be touched.
        assert_eq!(kill(&[1, 4]), 0);
    }

    #[test]
    fn start_filter_excludes_the_future() {
        // Nothing can have been started after "now + 1 hour".
        let future = now_filetime() + 36_000_000_000;
        assert!(client_pids_started_after(future).is_empty());
    }
}
