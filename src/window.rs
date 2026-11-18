//! Win32 window sizing for the Roblox client we just spawned.
//!
//! The client has no command-line switch for its window size, so the window is
//! resized after it appears. The client window belongs to a process we started,
//! therefore it is found by process id, not by title.
//!
//! One resize is not enough: the first window is the small login box, and the
//! client sizes the real one from its own saved settings once the place starts
//! loading. The size is therefore pinned for a while instead of applied once.

use std::time::{Duration, Instant};

/// How long the requested size is enforced after launch. Loading a place takes
/// tens of seconds on a cold start, and the client changes the window while it
/// does so; after this window the size is the user's own again.
const PIN_FOR: Duration = Duration::from_secs(90);
/// How often the size is checked while pinned. Cheap: one `GetClientRect`.
const PIN_POLL: Duration = Duration::from_millis(400);

/// Keep the first visible top-level window of `pid` at a `width`x`height`
/// **client area**. Runs on a short-lived thread: the client needs a moment to
/// create the window, and the caller must not block.
pub fn resize_when_ready(pid: u32, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    // Anything smaller is not a playable window; the UI never sends this.
    let width = width.max(320);
    let height = height.max(240);

    let spawned = std::thread::Builder::new()
        .name("rbm-window".into())
        .spawn(move || pin(pid, width, height));

    // A failed thread only means the window keeps its default size.
    let _ = spawned;
}

fn pin(pid: u32, width: u32, height: u32) {
    let deadline = Instant::now() + PIN_FOR;
    while Instant::now() < deadline {
        // The client may replace its window while loading, so the handle is
        // looked up again on every pass instead of being remembered.
        match win::find(pid) {
            Some(hwnd) if win::client_size(hwnd) != (width as i32, height as i32) => {
                win::resize(hwnd, width, height);
            }
            // Already right, or not created yet.
            _ => std::thread::sleep(PIN_POLL),
        }
    }
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::time::Duration;

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

    struct Search {
        pid: u32,
        found: Hwnd,
    }

    extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` points to a `Search` alive for the whole enumeration.
        let search = unsafe { &mut *(lparam as *mut Search) };

        let mut pid = 0u32;
        // SAFETY: `hwnd` is a window handle provided by EnumWindows.
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != search.pid || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }

        // Require a captioned, titled window: the client has some hidden
        // helper windows without a frame.
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        if style & WS_CAPTION == 0 || unsafe { GetWindowTextLengthW(hwnd) } == 0 {
            return 1;
        }

        search.found = hwnd;
        0 // stop the enumeration
    }

    pub(super) fn find(pid: u32) -> Option<Hwnd> {
        let mut search = Search {
            pid,
            found: std::ptr::null_mut(),
        };
        // SAFETY: the callback only writes into `search`, which outlives the call.
        unsafe {
            EnumWindows(visit, &mut search as *mut Search as isize);
        }
        (!search.found.is_null()).then_some(search.found)
    }

    /// Size of the drawable area, i.e. what the user actually sees.
    pub(super) fn client_size(hwnd: Hwnd) -> (i32, i32) {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `hwnd` came from `find` and is still alive.
        unsafe {
            GetClientRect(hwnd, &mut rect);
        }
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    /// `SetWindowPos` takes the outer size, clients count the inner one.
    pub(super) fn resize(hwnd: Hwnd, width: u32, height: u32) {
        // SAFETY: `hwnd` came from a successful search and is still alive.
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

            // The client reacts to WM_SIZE asynchronously; a short pause keeps
            // the loop from firing dozens of resizes at one window.
            std::thread::sleep(Duration::from_millis(150));
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn find(_pid: u32) -> Option<()> {
        None
    }

    pub(super) fn client_size(_window: ()) -> (i32, i32) {
        (0, 0)
    }

    pub(super) fn resize(_window: (), _width: u32, _height: u32) {}
}
