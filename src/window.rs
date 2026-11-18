//! Win32 window sizing for the Roblox client we just spawned.
//!
//! The client has no command-line switch for its window size, so the window is
//! resized after it appears. The window is found by process, not by title.
//!
//! One resize is not enough: the first window is the small login box, and the
//! client sizes the real one from its own saved settings once the place starts
//! loading. The client may even move the game into a freshly started process, so
//! every client process started after our launch is covered, not only the one we
//! spawned. The size is pinned for a while instead of applied once.

use std::time::{Duration, Instant};

/// How long the requested size is enforced after launch. Loading a place takes
/// tens of seconds on a cold start, and the client changes the window while it
/// does so; after this window the size is the user's own again.
const PIN_FOR: Duration = Duration::from_secs(90);
/// How often the size is checked while pinned. Cheap: one `GetClientRect`.
const PIN_POLL: Duration = Duration::from_millis(400);

/// Keep every window of the client at a `width`x`height` **client area**.
/// Runs on a short-lived thread: the client needs a moment to create the window,
/// and the caller must not block.
pub fn resize_when_ready(pid: u32, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    // The client refuses to go below an 800x600 client area: its own log shows
    // the swapchain staying at 800x599 whatever size is requested. Asking for
    // less only makes the pin fight the window for 90 seconds.
    let width = width.max(800);
    let height = height.max(600);

    let spawned = std::thread::Builder::new()
        .name("rbm-window".into())
        .spawn(move || pin(pid, width, height));

    // A failed thread only means the window keeps its default size.
    let _ = spawned;
}

fn pin(pid: u32, width: u32, height: u32) {
    // Clients started before us belong to the user, not to this launch.
    let since = win::now_filetime();
    let deadline = Instant::now() + PIN_FOR;
    while Instant::now() < deadline {
        let mut fixed = win::resize_all(pid, width, height);
        for other in win::client_pids_started_after(since) {
            if other != pid {
                fixed += win::resize_all(other, width, height);
            }
        }

        // Give the client time to react to WM_SIZE before the next pass.
        std::thread::sleep(if fixed > 0 {
            Duration::from_millis(250)
        } else {
            PIN_POLL
        });
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::time::{SystemTime, UNIX_EPOCH};

    type Hwnd = *mut c_void;
    type Handle = *mut c_void;

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
    const CLIENT_EXE: &str = "RobloxPlayerBeta.exe";

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(callback: extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindowTextLengthW(hwnd: Hwnd) -> i32;
        fn GetWindowLongW(hwnd: Hwnd, index: i32) -> i32;
        fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn AdjustWindowRectEx(rect: *mut Rect, style: u32, menu: i32, ex_style: u32) -> i32;
        fn SetWindowPos(
            hwnd: Hwnd,
            insert_after: Hwnd,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            flags: u32,
        ) -> i32;
    }

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

    const GWL_STYLE: i32 = -16;
    const GWL_EXSTYLE: i32 = -20;
    const WS_CAPTION: u32 = 0x00C0_0000;
    const SW_RESTORE: i32 = 9;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    const SWP_FRAMECHANGED: u32 = 0x0020;

    struct Pass {
        pid: u32,
        width: u32,
        height: u32,
        fixed: usize,
    }

    extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` points to a `Pass` alive for the whole enumeration.
        let pass = unsafe { &mut *(lparam as *mut Pass) };

        let mut pid = 0u32;
        // SAFETY: `hwnd` is a window handle provided by EnumWindows.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != pass.pid || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }

        // Require a captioned, titled window: the client has some hidden
        // helper windows without a frame.
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        if style & WS_CAPTION == 0 || unsafe { GetWindowTextLengthW(hwnd) } == 0 {
            return 1;
        }
        if client_size(hwnd) == (pass.width as i32, pass.height as i32) {
            return 1;
        }

        resize(hwnd, pass.width, pass.height);
        pass.fixed += 1;
        1 // keep going: the process may own more than one window
    }

    /// Bring every visible window of `pid` to the requested client area.
    /// Returns how many windows were touched.
    pub(super) fn resize_all(pid: u32, width: u32, height: u32) -> usize {
        let mut pass = Pass {
            pid,
            width,
            height,
            fixed: 0,
        };
        // SAFETY: the callback only writes into `pass`, which outlives the call.
        unsafe {
            EnumWindows(visit, &mut pass as *mut Pass as isize);
        }
        pass.fixed
    }

    /// Current time as a FILETIME tick count, for comparing process start times.
    pub(super) fn now_filetime() -> u64 {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        since.as_secs() * 10_000_000
            + u64::from(since.subsec_nanos()) / 100
            + 116_444_736_000_000_000
    }

    /// Ids of client processes started at or after `since`.
    ///
    /// The client can hand the game window to a process started after ours, so
    /// knowing only the pid we spawned is not enough.
    pub(super) fn client_pids_started_after(since: u64) -> Vec<u32> {
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
            if is_client(&entry) && started_after(entry.process_id, since) {
                pids.push(entry.process_id);
            }
            more = unsafe { Process32NextW(snapshot, &mut entry) };
        }

        unsafe { CloseHandle(snapshot) };
        pids
    }

    fn is_client(entry: &ProcessEntry32W) -> bool {
        let len = entry
            .exe_file
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(entry.exe_file.len());
        let name = String::from_utf16_lossy(&entry.exe_file[..len]);
        name.eq_ignore_ascii_case(CLIENT_EXE)
    }

    fn started_after(pid: u32, since: u64) -> bool {
        // SAFETY: the handle is closed before returning.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut creation = FileTime { low: 0, high: 0 };
        let mut exit = FileTime { low: 0, high: 0 };
        let mut kernel = FileTime { low: 0, high: 0 };
        let mut user = FileTime { low: 0, high: 0 };
        let ok =
            unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe { CloseHandle(process) };
        ok != 0 && creation.ticks() >= since
    }

    /// Size of the drawable area, i.e. what the user actually sees.
    fn client_size(hwnd: Hwnd) -> (i32, i32) {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `hwnd` came from `EnumWindows` in the same pass.
        unsafe {
            GetClientRect(hwnd, &mut rect);
        }
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    /// `SetWindowPos` takes the outer size, clients count the inner one.
    fn resize(hwnd: Hwnd, width: u32, height: u32) {
        // SAFETY: `hwnd` came from `EnumWindows` in the same pass.
        unsafe {
            // A maximized or minimized window ignores SetWindowPos.
            ShowWindow(hwnd, SW_RESTORE);

            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;

            let mut rect = Rect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            if AdjustWindowRectEx(&mut rect, style, 0, ex_style) == 0 {
                rect.right = width as i32;
                rect.bottom = height as i32;
            }

            // SWP_NOMOVE: only the size is ours to change. Forcing (0, 0) piled
            // every window we resized onto the same top-left corner.
            let flags = SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED;
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                flags,
            );
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn resize_all(_pid: u32, _width: u32, _height: u32) -> usize {
        0
    }

    pub(super) fn now_filetime() -> u64 {
        0
    }

    pub(super) fn client_pids_started_after(_since: u64) -> Vec<u32> {
        Vec::new()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn filetime_now_is_plausible() {
        // 2020-01-01 as FILETIME ticks; anything sane is far above it.
        let y2020 = 132_223_104_000_000_000u64;
        assert!(win::now_filetime() > y2020);
    }

    #[test]
    fn process_scan_is_safe() {
        // Whether or not the client runs on this machine, the scan must finish
        // and return real ids instead of garbage from a wrong struct layout.
        let pids = win::client_pids_started_after(0);
        assert!(pids.iter().all(|pid| *pid > 4));
    }
}
