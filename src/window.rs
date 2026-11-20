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
//!
//! The client answers `WM_WINDOWPOSCHANGING` with its own minimum size (an 800×600
//! client area). `SWP_NOSENDCHANGING` skips that message entirely, so a smaller
//! window is possible — which is what makes six windows fit on one screen. The
//! game then renders into a smaller area; the client logs `dims=800x599` for its
//! own swapchain but the window really is smaller.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::processes;

/// How long the requested size is enforced after launch. Loading a place takes
/// tens of seconds on a cold start, and the client changes the window while it
/// does so; after this window the size is the user's own again.
const PIN_FOR: Duration = Duration::from_secs(90);
/// How often the size is checked while pinned. Cheap: one `GetClientRect`.
const PIN_POLL: Duration = Duration::from_millis(400);

/// How long the grid keeps arranging windows after the last launch. A place load
/// can move the game into another process, so a few passes are needed.
const TILE_FOR: Duration = Duration::from_secs(45);
/// How often the grid is re-applied while active.
const TILE_POLL: Duration = Duration::from_millis(700);

/// Keep every window of the client at a `width`x`height` **client area**.
/// Runs on a short-lived thread: the client needs a moment to create the window,
/// and the caller must not block.
pub fn resize_when_ready(pid: u32, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }

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

/// Handles (as plain integers) of the game window of every client process.
/// Used by anti-AFK, which has to address each window.
pub fn game_window_handles() -> Vec<isize> {
    win::game_window_handles()
}

/// Deadline shared by every caller of [`arrange`].
static TILE_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// Set once so the layout thread is started only one time.
static TILE_STARTED: OnceLock<()> = OnceLock::new();

/// Spread all client windows over the screen in a grid.
///
/// Called after every launch: the number of windows changes, so the grid is
/// recomputed each time. A background thread keeps re-applying it for
/// [`TILE_FOR`], because the client moves its window while a place loads.
/// Cheap enough to call on every launch.
pub fn arrange() {
    {
        let Ok(mut until) = TILE_UNTIL.lock() else {
            return;
        };
        *until = Some(Instant::now() + TILE_FOR);
    }

    TILE_STARTED.get_or_init(|| {
        let spawned = std::thread::Builder::new()
            .name("rbm-tile".into())
            .spawn(tile_loop);
        // A failed thread only means the windows keep their own positions.
        let _ = spawned;
    });
}

/// Applies the grid while a plan is active and idles the rest of the time.
fn tile_loop() {
    loop {
        let active = TILE_UNTIL
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until);

        if active {
            win::tile_all();
            std::thread::sleep(TILE_POLL);
        } else {
            std::thread::sleep(Duration::from_millis(300));
        }
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
        fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn SystemParametersInfoW(action: u32, param: u32, data: *mut c_void, win_ini: u32) -> i32;
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
    const WS_CAPTION: u32 = 0x00C0_0000;
    const SW_RESTORE: i32 = 9;
    const SWP_NOMOVE: u32 = 0x0002;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_NOACTIVATE: u32 = 0x0010;
    /// The window never gets `WM_WINDOWPOSCHANGING`, so it cannot clamp the size
    /// to its own minimum.
    const SWP_NOSENDCHANGING: u32 = 0x0400;
    const SPI_GETWORKAREA: u32 = 0x0030;

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

        resize(hwnd, pass.width, pass.height, None);
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

    /// One visible, captioned client window.
    struct Found {
        hwnd: Hwnd,
        pid: u32,
    }

    struct Collect {
        found: Vec<Found>,
    }

    extern "system" fn collect(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` points to a `Collect` alive for the whole enumeration.
        let list = unsafe { &mut *(lparam as *mut Collect) };
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
        if style & WS_CAPTION == 0 || unsafe { GetWindowTextLengthW(hwnd) } == 0 {
            return 1;
        }

        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid == 0 {
            return 1;
        }
        list.found.push(Found { hwnd, pid });
        1
    }

    /// Screen area free of the taskbar, in physical pixels.
    fn work_area() -> Rect {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: the struct is a plain RECT and lives for the call.
        let ok = unsafe {
            SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rect as *mut Rect as *mut c_void, 0)
        };
        if ok == 0 || rect.right <= rect.left || rect.bottom <= rect.top {
            // A sane fallback: 1920x1080 minus a taskbar.
            return Rect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            };
        }
        rect
    }

    /// Rows and columns that give each window the closest shape to the screen.
    ///
    /// Six windows on a 16:9 screen become 3x2, four become 2x2, three stay in
    /// one row. The caller passes what is actually running, so the grid follows
    /// the number of accounts instead of a fixed preset.
    pub(super) fn grid(count: usize, screen_width: i32, screen_height: i32) -> (usize, usize) {
        if count <= 1 {
            return (1, 1);
        }
        let aspect = screen_width.max(1) as f32 / screen_height.max(1) as f32;

        let mut best = (count, 1);
        let mut best_score = f32::MAX;
        for rows in 1..=count {
            let cols = count.div_ceil(rows);
            let cell = aspect / (cols as f32 / rows as f32);
            // Prefer square-ish cells, and a grid that is exactly full.
            let waste = (rows * cols - count) as f32;
            let score = (cell.max(1.0 / cell)).ln().abs() + waste * 0.15;
            if score < best_score {
                best_score = score;
                best = (rows, cols);
            }
        }
        // `best` is (rows, cols); the callers read it as (columns, rows).
        (best.1, best.0)
    }

    /// The game window of every client process: one per process, the largest.
    fn game_windows() -> Vec<Found> {
        let mut list = Collect { found: Vec::new() };
        // SAFETY: the callback only writes into `list`, which outlives the call.
        unsafe {
            EnumWindows(collect, &mut list as *mut Collect as isize);
        }

        let clients: std::collections::HashSet<u32> =
            crate::processes::client_pids().into_iter().collect();
        // One window per client process: the client also owns short-lived splash
        // windows, and a second one would take a cell of the grid for nothing.
        let mut best: std::collections::HashMap<u32, (i32, Found)> =
            std::collections::HashMap::new();
        for found in list.found {
            if !clients.contains(&found.pid) {
                continue;
            }
            let (width, height) = client_size(found.hwnd);
            let area = width.max(0) * height.max(0);
            // Too small to be the game window.
            if area < 100 * 100 {
                continue;
            }
            match best.get(&found.pid) {
                Some((known, _)) if *known >= area => {}
                _ => {
                    best.insert(found.pid, (area, found));
                }
            }
        }
        best.into_values().map(|(_, found)| found).collect()
    }

    pub(super) fn game_window_handles() -> Vec<isize> {
        game_windows()
            .into_iter()
            .map(|found| found.hwnd as isize)
            .collect()
    }

    /// Arrange every visible client window in a grid over the work area.
    pub(super) fn tile_all() {
        let mut windows = game_windows();
        if windows.is_empty() {
            return;
        }
        // Older pid first: the client is started in launch order, so the grid
        // follows the order the accounts were started in.
        windows.sort_by_key(|found| found.pid);

        let area = work_area();
        let (columns, rows) = grid(
            windows.len(),
            area.right - area.left,
            area.bottom - area.top,
        );
        let columns = columns.max(1) as i32;
        let rows = rows.max(1) as i32;

        let width = (area.right - area.left) / columns;
        let height = (area.bottom - area.top) / rows;

        for (index, found) in windows.iter().enumerate() {
            let column = index as i32 % columns;
            let row = index as i32 / columns % rows;
            let x = area.left + column * width;
            let y = area.top + row * height;
            resize(found.hwnd, width as u32, height as u32, Some((x, y)));
        }
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

    /// Frame thickness of a window: outer size minus client size.
    fn frame_of(hwnd: Hwnd) -> (i32, i32) {
        let mut outer = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `hwnd` is a live window handle.
        let ok = unsafe { GetWindowRect(hwnd, &mut outer) };
        if ok == 0 {
            return (0, 0);
        }
        let (client_w, client_h) = client_size(hwnd);
        (
            (outer.right - outer.left - client_w).max(0),
            (outer.bottom - outer.top - client_h).max(0),
        )
    }

    /// `SetWindowPos` takes the outer size, clients count the inner one.
    fn resize(hwnd: Hwnd, width: u32, height: u32, position: Option<(i32, i32)>) {
        // SAFETY: `hwnd` came from `EnumWindows` in the same pass.
        unsafe {
            // A maximized or minimized window ignores SetWindowPos.
            ShowWindow(hwnd, SW_RESTORE);

            let (frame_w, frame_h) = frame_of(hwnd);
            let outer_w = width as i32 + frame_w;
            let outer_h = height as i32 + frame_h;

            let (x, y, flags) = match position {
                Some((x, y)) => (x, y, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSENDCHANGING),
                // SWP_NOMOVE: only the size is ours to change. Forcing (0, 0)
                // piled every window we resized onto the same top-left corner.
                None => (
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSENDCHANGING,
                ),
            };
            SetWindowPos(hwnd, std::ptr::null_mut(), x, y, outer_w, outer_h, flags);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::grid;

        #[test]
        fn one_window_fills_the_screen() {
            assert_eq!(grid(1, 1920, 1080), (1, 1));
        }

        #[test]
        fn six_windows_become_three_by_two() {
            // Six accounts on a 16:9 screen: two rows of three keeps the cells
            // wider than tall, which is what the client draws best.
            assert_eq!(grid(6, 1920, 1080), (3, 2));
        }

        #[test]
        fn four_windows_become_a_square() {
            assert_eq!(grid(4, 1920, 1080), (2, 2));
        }

        #[test]
        fn five_windows_fill_the_first_row_first() {
            let (columns, rows) = grid(5, 1920, 1080);
            assert!(columns * rows >= 5, "{columns}x{rows}");
            assert!((columns * rows - 5) <= 1, "{columns}x{rows}");
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn resize_all(_pid: u32, _width: u32, _height: u32) -> usize {
        0
    }

    pub(super) fn tile_all() {}

    pub(super) fn game_window_handles() -> Vec<isize> {
        Vec::new()
    }
}
