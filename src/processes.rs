//! Which Roblox processes are running, and which one is in front.
//!
//! The client can move its game window into a process started after the one we
//! spawned, so the manager has to look at processes, not at a single pid. Both
//! the window pinner and the memory trimmer need this list.
//!
//! On Linux the client is Sober, and one account is one Flatpak instance: the
//! app runs several `sober` processes that all share its cgroup scope, so the
//! scan groups them and reports one entry per instance.

/// Executable name of the game client.
#[cfg_attr(not(windows), allow(dead_code))]
const CLIENT_EXE: &str = "RobloxPlayerBeta.exe";

/// Ids of running client processes.
pub fn client_pids() -> Vec<u32> {
    win::client_pids(0)
}

/// Ids of client processes started at or after `since`, a FILETIME tick count.
#[allow(dead_code)]
pub fn client_pids_started_after(since: u64) -> Vec<u32> {
    win::client_pids(since)
}

/// Current time as FILETIME ticks (100 ns since 1601), comparable with process
/// creation times.
pub fn now_filetime() -> u64 {
    win::now_filetime()
}

/// Pid that owns the foreground window, if any.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn foreground_pid() -> Option<u32> {
    win::foreground_pid()
}

/// Linux cgroup v2 path of the instance `pid` belongs to, without the
/// `/sys/fs/cgroup` prefix: `/user.slice/.../app-flatpak-org.vinegarhq.Sober-…`.
/// `None` on Windows and for a process that has already gone.
pub fn cgroup_path(pid: u32) -> Option<String> {
    win::cgroup_path(pid)
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
        .filter(|pid| win::kill_instance(**pid))
        .count()
}

/// End every client window. Returns how many were ended.
#[cfg_attr(not(windows), allow(dead_code))]
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

    pub(super) fn cgroup_path(_pid: u32) -> Option<String> {
        None
    }

    /// `taskkill /F` on the pid the tracker holds for the window.
    pub(super) fn kill_instance(pid: u32) -> bool {
        crate::roblox::run_hidden(std::process::Command::new("taskkill").args([
            "/F",
            "/PID",
            &pid.to_string(),
        ]))
        .is_some()
    }
}

/// Linux: the game runs inside Flatpak, so `/proc` is the whole story.
///
/// One Sober instance is several processes (`sober`, a render helper, `bwrap`
/// supervisors) in one systemd scope named after the app. Grouping by cgroup is
/// what turns them back into "one window"; the leader (`pid == session`) is the
/// process the tracker remembers and the one a later kill is aimed at.
#[cfg(not(windows))]
mod win {
    use std::collections::HashMap;
    use std::fs;
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `USER_HZ` is 100 on every Linux ABI Rust builds for.
    const HZ: u64 = 100;
    const TICKS: u64 = 10_000_000;
    /// Substring of every cgroup that belongs to the Sober sandbox.
    const CGROUP_MARK: &str = "app-flatpak-org.vinegarhq.Sober";
    /// Processes that make up the app itself, as opposed to helpers that only
    /// borrow its cgroup (a login window, for instance, is one of ours too).
    const APP_COMMS: [&str; 3] = ["sober", "sober_services", "bwrap"];

    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    const SIGTERM: i32 = 15;

    /// One Sober instance found in `/proc`.
    struct Instance {
        /// Pid that leads the instance, or the oldest `sober` process in it.
        leader: u32,
        /// FILETIME ticks of the leader's creation.
        created: u64,
        /// True when the leader is the app's session leader.
        main: bool,
    }

    fn read_trimmed(path: String) -> Option<String> {
        let text = fs::read_to_string(path).ok()?;
        Some(text.trim_end().to_string())
    }

    /// First line of `/proc/<pid>/cgroup`: `0::/user.slice/...scope`.
    fn cgroup_of(pid: u32) -> Option<String> {
        let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        text.lines().next().map(|line| line.trim().to_string())
    }

    /// The same line without the `0::` prefix: the path below the cgroup root.
    pub(super) fn cgroup_path(pid: u32) -> Option<String> {
        let line = cgroup_of(pid)?;
        let (_, path) = line.split_once("::")?;
        (!path.is_empty()).then(|| path.to_string())
    }

    /// Fields after `(comm)` in `/proc/<pid>/stat`: state, ppid, pgrp, session…
    fn stat_rest(pid: u32) -> Option<Vec<String>> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let close = text.rfind(')')?;
        Some(
            text[close + 1..]
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        )
    }

    /// Session id of the process, used to spot the instance's main process.
    fn session_of(pid: u32) -> Option<u32> {
        stat_rest(pid)?.get(3)?.parse().ok()
    }

    /// Start time in clock ticks since boot.
    fn start_ticks(pid: u32) -> Option<u64> {
        stat_rest(pid)?.get(19)?.parse().ok()
    }

    /// Seconds since boot, with the fraction `/proc/uptime` gives.
    fn uptime_secs() -> f64 {
        fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|text| {
                text.split_whitespace()
                    .next()
                    .and_then(|first| first.parse().ok())
            })
            .unwrap_or(0.0)
    }

    /// FILETIME ticks for a wall-clock instant, same base as the Windows code.
    fn system_filetime() -> u64 {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        since.as_secs() * TICKS + u64::from(since.subsec_nanos()) / 100 + 116_444_736_000_000_000
    }

    /// FILETIME ticks at boot. Taken once, then advanced with `/proc/uptime`,
    /// which is monotonic and therefore can never disagree with a start time
    /// taken from the same counter.
    fn boot_filetime() -> u64 {
        static BOOT: OnceLock<u64> = OnceLock::new();
        *BOOT
            .get_or_init(|| system_filetime().saturating_sub((uptime_secs() * TICKS as f64) as u64))
    }

    fn created_of(pid: u32) -> Option<u64> {
        let ticks = start_ticks(pid)?;
        Some(boot_filetime() + ticks * TICKS / HZ)
    }

    /// Every Sober instance on the machine, one entry each.
    fn instances() -> Vec<Instance> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };

        let mut groups: HashMap<String, Instance> = HashMap::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            if read_trimmed(format!("/proc/{pid}/comm")).as_deref() != Some("sober") {
                continue;
            }
            let Some(cgroup) = cgroup_of(pid) else {
                continue;
            };
            if !cgroup.contains(CGROUP_MARK) {
                continue;
            }
            let Some(created) = created_of(pid) else {
                continue;
            };
            let main = session_of(pid) == Some(pid);

            let seat = groups.entry(cgroup).or_insert(Instance {
                leader: pid,
                created,
                main,
            });
            // The session leader wins; otherwise the oldest process does.
            if (main && !seat.main) || (main == seat.main && created < seat.created) {
                seat.leader = pid;
                seat.created = created;
                seat.main = main;
            }
        }

        let mut list: Vec<Instance> = groups.into_values().collect();
        list.sort_by_key(|instance| instance.created);
        list
    }

    pub(super) fn client_pids(since: u64) -> Vec<u32> {
        super::client_procs()
            .into_iter()
            .filter(|proc| since == 0 || proc.created >= since)
            .map(|proc| proc.pid)
            .collect()
    }

    pub(super) fn client_procs() -> Vec<super::Proc> {
        instances()
            .into_iter()
            .map(|instance| super::Proc {
                pid: instance.leader,
                created: instance.created,
            })
            .collect()
    }

    pub(super) fn now_filetime() -> u64 {
        boot_filetime() + (uptime_secs() * TICKS as f64) as u64
    }

    /// Wayland gives no global "active window"; KWin knows, but the pinner and
    /// the Windows trimmer are the only callers and neither runs here.
    #[allow(dead_code)]
    pub(super) fn foreground_pid() -> Option<u32> {
        None
    }

    /// End the whole instance the pid belongs to: the app spawns several
    /// processes per window, and they all have to go for the window to close.
    pub(super) fn kill_instance(pid: u32) -> bool {
        let Some(cgroup) = cgroup_of(pid) else {
            return false;
        };
        let mut hit = false;
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(other) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse().ok())
                else {
                    continue;
                };
                let Some(comm) = read_trimmed(format!("/proc/{other}/comm")) else {
                    continue;
                };
                if !APP_COMMS.contains(&comm.as_str()) {
                    continue;
                }
                if cgroup_of(other).as_deref() != Some(cgroup.as_str()) {
                    continue;
                }
                // SAFETY: plain signal to a pid the kernel just showed us.
                if unsafe { kill(other as i32, SIGTERM) } == 0 {
                    hit = true;
                }
            }
        }
        hit
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
