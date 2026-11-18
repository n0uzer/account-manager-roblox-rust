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

use crate::processes;

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
    let since = processes::now_filetime();
    let deadline = Instant::now() + PIN_FOR;
    while Instant::now() < deadline {
        let mut fixed = win::resize_all(pid, width, height);
        for other in processes::client_pids_started_after(since) {
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

    type Hwnd = *mut c_void;

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
}
