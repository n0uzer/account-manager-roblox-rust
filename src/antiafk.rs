//! Optional anti-AFK: a tiny input event now and then, so a window is not kicked
//! for sitting idle.
//!
//! Roblox removes a player who has sent no input for about 20 minutes. Every
//! `interval` minutes each client window gets a harmless input: a 1-pixel mouse
//! move and back and a press of F15, a key no game binds to anything. There are
//! two ways to deliver it ([`Mode`]):
//!
//! * **Focus** (default). The window is brought to the front for a fraction of a
//!   second, real input is sent with `SendInput`, and the window that was in
//!   front before is restored. This is what the client sees as a person at the
//!   keyboard, so it works with every client version. The price is a short flash
//!   of each window; the manager waits for a quiet moment first, so it does not
//!   steal focus in the middle of typing.
//! * **Quiet**. The same events are *posted* to the window (`PostMessageW`):
//!   nothing takes focus. Current clients mostly ignore input they did not get
//!   as the active window, so this is kept as an option, not the default.
//!
//! The client is not injected into and its memory is not touched. Off by default.
//!
//! Limits worth knowing:
//! * a client running as administrator ignores input from a normal-rights
//!   manager (Windows blocks that on purpose) — the report says how many windows
//!   could not be reached;
//! * it is still automation: some games and Roblox's rules treat that as a
//!   violation. Use at your own risk, see the README.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
use crate::window;

pub const MIN_MINUTES: u32 = 1;
/// Roblox kicks at 20 minutes, so a longer interval could not help.
pub const MAX_MINUTES: u32 = 19;
pub const DEFAULT_MINUTES: u32 = 10;

/// How often the worker looks at the clock.
const TICK: Duration = Duration::from_secs(5);

/// How the input reaches the window.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Bring the window to the front for an instant and send real input.
    #[default]
    Focus,
    /// Post messages to the window and leave the focus alone.
    Quiet,
}

impl Mode {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Mode::Focus => "надёжный: окно на миг выходит вперёд",
            Mode::Quiet => "тихий: без фокуса (у многих клиентов не срабатывает)",
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn variants() -> [Mode; 2] {
        [Mode::Focus, Mode::Quiet]
    }

    fn to_u8(self) -> u8 {
        match self {
            Mode::Focus => 0,
            Mode::Quiet => 1,
        }
    }

    fn from_u8(value: u8) -> Mode {
        if value == 1 {
            Mode::Quiet
        } else {
            Mode::Focus
        }
    }
}

/// What one round did, for the journal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Report {
    pub mode: Mode,
    /// Client windows found.
    pub windows: usize,
    /// Windows that took the input.
    pub reached: usize,
    /// Windows that could not be brought to the front (`Mode::Focus` only).
    pub not_focused: usize,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.windows > 0 && self.reached == self.windows
    }

    pub fn text(&self) -> String {
        if self.windows == 0 {
            return "Анти-AFK: окон клиента не найдено".to_string();
        }
        let mut text = format!(
            "Анти-AFK: ввод отправлен в окон {} из {}",
            self.reached, self.windows
        );
        if self.not_focused > 0 {
            text.push_str(&format!("; не вышли вперёд: {}", self.not_focused));
            #[cfg(windows)]
            text.push_str(" (клиент запущен от администратора?)");
        }
        text
    }
}

/// Whether anti-AFK can work on this platform right now.
///
/// Windows has `SendInput`. On Linux the window must be focused (Wayland has no
/// other way) and the tap must come from `uinput`; both halves are required.
pub fn supported() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        crate::kwin::available() && crate::uinput::available()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// How long the user must have been still before a window is pulled to the
/// front, in milliseconds.
#[cfg_attr(not(windows), allow(dead_code))]
const QUIET_MS: u32 = 1_500;
/// How long a round waits for such a moment before going ahead anyway.
#[cfg_attr(not(windows), allow(dead_code))]
const QUIET_WAIT: Duration = Duration::from_secs(5);

#[cfg_attr(not(windows), allow(dead_code))]
fn is_quiet(idle_ms: u32) -> bool {
    idle_ms >= QUIET_MS
}

/// Virtual-key code and scan code of F15.
#[cfg_attr(not(windows), allow(dead_code))]
const VK_F15: usize = 0x7E;
#[cfg_attr(not(windows), allow(dead_code))]
const SCAN_F15: u32 = 0x64;

static ENABLED: AtomicBool = AtomicBool::new(false);
static MINUTES: AtomicU32 = AtomicU32::new(DEFAULT_MINUTES);
static MODE: AtomicU8 = AtomicU8::new(0);
static STARTED: Once = Once::new();
/// A manual round is running; a second click waits for it.
static BUSY: AtomicBool = AtomicBool::new(false);
/// The result of the latest round, until the interface has shown it.
static LAST_REPORT: Mutex<Option<Report>> = Mutex::new(None);

pub fn clamp_minutes(minutes: u32) -> u32 {
    minutes.clamp(MIN_MINUTES, MAX_MINUTES)
}

/// Whether enough time has passed since the last pulse.
fn is_due(since_last: Duration, minutes: u32) -> bool {
    since_last >= Duration::from_secs(u64::from(clamp_minutes(minutes)) * 60)
}

/// Apply the settings. Starts the worker the first time anti-AFK is turned on.
pub fn configure(enabled: bool, minutes: u32, mode: Mode) {
    MINUTES.store(clamp_minutes(minutes), Ordering::Relaxed);
    MODE.store(mode.to_u8(), Ordering::Relaxed);
    ENABLED.store(enabled, Ordering::Relaxed);
    if enabled {
        start_worker();
    }
}

fn start_worker() {
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("rbm-antiafk".into())
            .spawn(|| {
                let mut last = Instant::now();
                loop {
                    std::thread::sleep(TICK);
                    if !ENABLED.load(Ordering::Relaxed) {
                        // Turning it back on waits a full interval instead of
                        // firing at once.
                        last = Instant::now();
                        continue;
                    }
                    if is_due(last.elapsed(), MINUTES.load(Ordering::Relaxed)) {
                        pulse(Mode::from_u8(MODE.load(Ordering::Relaxed)));
                        last = Instant::now();
                    }
                }
            });
        // A failed thread only means no anti-AFK.
        let _ = spawned;
    });
}

/// Send one round of input to every client window and remember what happened.
/// Blocks for a fraction of a second per window in `Mode::Focus`.
pub fn pulse(mode: Mode) -> Report {
    #[cfg(windows)]
    let (windows, reached, not_focused) = {
        let handles = window::game_window_handles();
        match mode {
            Mode::Quiet => (
                handles.len(),
                handles.iter().filter(|hwnd| win::nudge(**hwnd)).count(),
                0,
            ),
            Mode::Focus => {
                let (reached, not_focused) = win::focus_pulse(&handles);
                (handles.len(), reached, not_focused)
            }
        }
    };
    // Wayland has no quiet mode: input only reaches the focused window, and the
    // focus is what the pulse borrows for a moment.
    #[cfg(not(windows))]
    let (windows, reached, not_focused) = win::focus_pulse(&[]);

    let report = Report {
        mode,
        windows,
        reached,
        not_focused,
    };
    if let Ok(mut last) = LAST_REPORT.lock() {
        *last = Some(report);
    }
    report
}

/// One round on its own thread, for the "now" button: the interface must not
/// wait for windows to be brought forward one by one. The result comes back
/// through [`take_report`].
pub fn pulse_in_background(mode: Mode) {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rbm-antiafk-now".into())
        .spawn(move || {
            pulse(mode);
            BUSY.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        BUSY.store(false, Ordering::Release);
    }
}

/// The result of the latest round, once. `None` when nothing new happened.
pub fn take_report() -> Option<Report> {
    LAST_REPORT.lock().ok().and_then(|mut last| last.take())
}

/// `lParam` of a mouse message: x in the low word, y in the high word.
#[cfg_attr(not(windows), allow(dead_code))]
fn mouse_lparam(x: i32, y: i32) -> isize {
    (((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF)) as isize
}

/// `lParam` of `WM_KEYDOWN` / `WM_KEYUP`: repeat count, scan code and the
/// previous-state and transition bits a real key press carries.
#[cfg_attr(not(windows), allow(dead_code))]
fn key_lparam(scan: u32, release: bool) -> isize {
    let mut value = 1 | (scan << 16);
    if release {
        value |= (1 << 30) | (1 << 31);
    }
    value as isize
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    type Hwnd = *mut c_void;

    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct MouseInput {
        dx: i32,
        dy: i32,
        data: u32,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyInput {
        vk: u16,
        scan: u16,
        flags: u32,
        time: u32,
        extra: usize,
    }

    #[repr(C)]
    union InputUnion {
        mouse: MouseInput,
        key: KeyInput,
    }

    /// `INPUT`: 40 bytes on 64-bit Windows, 28 on 32-bit. The union is as large
    /// as its biggest member, which is what `SendInput` checks the size against.
    #[repr(C)]
    struct Input {
        kind: u32,
        body: InputUnion,
    }

    #[repr(C)]
    struct LastInput {
        size: u32,
        time: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
        fn GetForegroundWindow() -> Hwnd;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn AttachThreadInput(thread: u32, attach_to: u32, attach: i32) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
        fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
        fn GetLastInputInfo(info: *mut LastInput) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn GetTickCount() -> u32;
    }

    const INPUT_MOUSE: u32 = 0;
    const INPUT_KEYBOARD: u32 = 1;
    const MOUSEEVENTF_MOVE: u32 = 0x0001;
    const KEYEVENTF_KEYUP: u32 = 0x0002;
    const SW_RESTORE: i32 = 9;
    /// Minimize without activating, to put a restored window back.
    const SW_SHOWMINNOACTIVE: i32 = 7;

    const WM_KEYDOWN: u32 = 0x0100;
    const WM_KEYUP: u32 = 0x0101;
    const WM_MOUSEMOVE: u32 = 0x0200;

    /// Post the input to one window. False when the window refused all of it
    /// (it is gone, or runs with higher rights).
    pub(super) fn nudge(handle: isize) -> bool {
        let hwnd = handle as Hwnd;

        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: a plain RECT that lives for the call; a stale handle just fails.
        let known = unsafe { GetClientRect(hwnd, &mut rect) } != 0;
        let (x, y) = if known {
            ((rect.right - rect.left) / 2, (rect.bottom - rect.top) / 2)
        } else {
            (100, 100)
        };

        let steps: [(u32, usize, isize); 4] = [
            (WM_MOUSEMOVE, 0, super::mouse_lparam(x + 1, y)),
            (WM_MOUSEMOVE, 0, super::mouse_lparam(x, y)),
            (
                WM_KEYDOWN,
                super::VK_F15,
                super::key_lparam(super::SCAN_F15, false),
            ),
            (
                WM_KEYUP,
                super::VK_F15,
                super::key_lparam(super::SCAN_F15, true),
            ),
        ];

        let mut delivered = false;
        for (message, wparam, lparam) in steps {
            // SAFETY: posting a message copies its parameters; nothing is read
            // through them.
            delivered |= unsafe { PostMessageW(hwnd, message, wparam, lparam) } != 0;
        }
        delivered
    }

    /// Milliseconds since the last keyboard or mouse input of the user.
    fn idle_millis() -> Option<u32> {
        let mut info = LastInput {
            size: std::mem::size_of::<LastInput>() as u32,
            time: 0,
        };
        // SAFETY: a plain struct with its size filled in, alive for the call.
        if unsafe { GetLastInputInfo(&mut info) } == 0 {
            return None;
        }
        // SAFETY: no arguments, no side effects.
        let now = unsafe { GetTickCount() };
        Some(now.wrapping_sub(info.time))
    }

    /// Wait (a few seconds at most) until the user has stopped typing or moving
    /// the mouse, so the focus is not taken in the middle of a word.
    fn wait_for_quiet() {
        let until = Instant::now() + super::QUIET_WAIT;
        while Instant::now() < until {
            match idle_millis() {
                Some(idle) if !super::is_quiet(idle) => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => return,
            }
        }
    }

    /// Make `hwnd` the foreground window. Windows lets only the foreground
    /// thread do that, so this thread borrows its input queue for a moment.
    fn bring_to_front(hwnd: Hwnd) -> bool {
        // SAFETY: plain window and thread handles; a stale one just fails.
        unsafe {
            let front = GetForegroundWindow();
            if front == hwnd {
                return true;
            }
            let me = GetCurrentThreadId();
            let front_thread = if front.is_null() {
                0
            } else {
                GetWindowThreadProcessId(front, std::ptr::null_mut())
            };
            let attached = front_thread != 0 && front_thread != me && {
                AttachThreadInput(me, front_thread, 1) != 0
            };
            SetForegroundWindow(hwnd);
            if attached {
                AttachThreadInput(me, front_thread, 0);
            }
            GetForegroundWindow() == hwnd
        }
    }

    /// A 1-pixel mouse move and back, then F15 down and up, as real input.
    fn send_input() -> bool {
        let mouse = |dx: i32| Input {
            kind: INPUT_MOUSE,
            body: InputUnion {
                mouse: MouseInput {
                    dx,
                    dy: 0,
                    data: 0,
                    flags: MOUSEEVENTF_MOVE,
                    time: 0,
                    extra: 0,
                },
            },
        };
        let key = |flags: u32| Input {
            kind: INPUT_KEYBOARD,
            body: InputUnion {
                key: KeyInput {
                    vk: super::VK_F15 as u16,
                    scan: super::SCAN_F15 as u16,
                    flags,
                    time: 0,
                    extra: 0,
                },
            },
        };
        let inputs = [mouse(1), mouse(-1), key(0), key(KEYEVENTF_KEYUP)];
        // SAFETY: the array lives for the call and the size is that of `Input`.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<Input>() as i32,
            )
        };
        sent as usize == inputs.len()
    }

    /// Bring each window forward in turn, send it input, and put the window that
    /// was in front before back. Returns (windows reached, windows that could
    /// not be brought forward).
    pub(super) fn focus_pulse(handles: &[isize]) -> (usize, usize) {
        if handles.is_empty() {
            return (0, 0);
        }
        wait_for_quiet();

        // SAFETY: no arguments; the result is only compared and handed back.
        let previous = unsafe { GetForegroundWindow() };
        let mut reached = 0;
        let mut not_focused = 0;

        for handle in handles {
            let hwnd = *handle as Hwnd;
            // A minimized window cannot be given the focus.
            // SAFETY: a stale handle just fails.
            let minimized = unsafe { IsIconic(hwnd) } != 0;
            if minimized {
                // SAFETY: as above.
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                std::thread::sleep(Duration::from_millis(150));
            }

            if bring_to_front(hwnd) {
                // The window needs a moment to become active and, afterwards, to
                // read what it was sent.
                std::thread::sleep(Duration::from_millis(80));
                if send_input() {
                    reached += 1;
                }
                std::thread::sleep(Duration::from_millis(120));
            } else {
                not_focused += 1;
            }

            if minimized {
                // SAFETY: as above.
                unsafe {
                    ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
                }
            }
        }

        if !previous.is_null() {
            bring_to_front(previous);
        }
        (reached, not_focused)
    }
}

#[cfg(not(windows))]
mod win {
    /// One round over the Sober windows KWin knows about. Each window gets the
    /// focus for a moment and a real F15 tap from the virtual keyboard; the
    /// window that was in front is restored afterwards.
    ///
    /// Returns (windows, reached, not focused).
    pub(super) fn focus_pulse(_handles: &[isize]) -> (usize, usize, usize) {
        #[cfg(target_os = "linux")]
        {
            use std::time::Duration;

            let windows = crate::kwin::windows();
            if windows.is_empty() {
                return (0, 0, 0);
            }
            let previous = crate::kwin::active_window();
            let mut reached = 0;
            let mut not_focused = 0;

            for window in &windows {
                if crate::kwin::focus(window) {
                    // The compositor needs a moment to route the keyboard, and
                    // the game a moment to read the event.
                    std::thread::sleep(Duration::from_millis(350));
                    if crate::uinput::tap_f15() {
                        reached += 1;
                    }
                    std::thread::sleep(Duration::from_millis(150));
                } else {
                    not_focused += 1;
                }
            }

            if let Some(previous) = previous {
                crate::kwin::focus(&previous);
            }
            (windows.len(), reached, not_focused)
        }
        #[cfg(not(target_os = "linux"))]
        {
            (0, 0, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interval_stays_below_the_kick_time() {
        assert_eq!(clamp_minutes(0), MIN_MINUTES);
        assert_eq!(clamp_minutes(10), 10);
        assert_eq!(clamp_minutes(20), MAX_MINUTES);
        assert_eq!(clamp_minutes(9_999), MAX_MINUTES);
    }

    // The client kicks at 20 minutes of idle; the longest interval must stay below.
    const _: () = assert!(MAX_MINUTES < 20);

    #[test]
    fn a_pulse_is_due_once_the_interval_has_passed() {
        assert!(!is_due(Duration::from_secs(9 * 60 + 59), 10));
        assert!(is_due(Duration::from_secs(10 * 60), 10));
        assert!(is_due(Duration::from_secs(3_600), 10));
        // An out-of-range setting is clamped, not trusted.
        assert!(is_due(Duration::from_secs(19 * 60), 500));
        assert!(!is_due(Duration::from_secs(30), 0));
    }

    #[test]
    fn mouse_coordinates_are_packed_low_word_first() {
        assert_eq!(mouse_lparam(0x12, 0x34), 0x0034_0012);
        assert_eq!(mouse_lparam(1, 0), 1);
        // Negative coordinates keep to 16 bits instead of spilling over.
        assert_eq!(mouse_lparam(-1, 0) as u32, 0x0000_FFFF);
    }

    #[test]
    fn key_flags_match_a_real_press_and_release() {
        let down = key_lparam(SCAN_F15, false) as u32;
        let up = key_lparam(SCAN_F15, true) as u32;
        assert_eq!(down & 0xFFFF, 1, "repeat count");
        assert_eq!((down >> 16) & 0xFF, SCAN_F15);
        assert_eq!(down >> 30, 0, "a press has neither flag");
        assert_eq!(up >> 30, 0b11, "a release carries both");
    }

    #[test]
    fn a_pulse_without_clients_reaches_nobody() {
        // The Linux pulse asks KWin for windows instead of taking a handle
        // list, so a real client running next to the tests would get the
        // input. Do not touch it: this test is about the empty case.
        if !crate::processes::client_procs().is_empty() {
            eprintln!("skipping: a live client is running");
            return;
        }
        for mode in Mode::variants() {
            let report = pulse(mode);
            assert_eq!((report.windows, report.reached), (0, 0));
            assert!(!report.is_ok());
        }
        assert!(take_report().is_some(), "the round leaves a report");
        assert!(take_report().is_none(), "and it is handed out only once");
    }

    #[test]
    fn settings_are_clamped_when_applied() {
        configure(false, 500, Mode::Quiet);
        assert_eq!(MINUTES.load(Ordering::Relaxed), MAX_MINUTES);
        assert_eq!(Mode::from_u8(MODE.load(Ordering::Relaxed)), Mode::Quiet);
        configure(false, DEFAULT_MINUTES, Mode::Focus);
        assert!(!ENABLED.load(Ordering::Relaxed));
    }

    #[test]
    fn the_mode_survives_its_stored_form_and_defaults_to_focus() {
        for mode in Mode::variants() {
            assert_eq!(Mode::from_u8(mode.to_u8()), mode);
        }
        assert_eq!(Mode::default(), Mode::Focus);
        assert_eq!(Mode::from_u8(200), Mode::Focus);
    }

    #[test]
    fn the_report_says_what_was_reached() {
        let all = Report {
            mode: Mode::Focus,
            windows: 3,
            reached: 3,
            not_focused: 0,
        };
        assert!(all.is_ok());
        assert_eq!(all.text(), "Анти-AFK: ввод отправлен в окон 3 из 3");

        let part = Report {
            reached: 1,
            not_focused: 2,
            ..all
        };
        assert!(!part.is_ok());
        assert!(part.text().contains("1 из 3"));
        assert!(part.text().contains("не вышли вперёд: 2"));

        let none = Report {
            windows: 0,
            reached: 0,
            ..all
        };
        assert!(none.text().contains("не найдено"));
    }

    #[test]
    fn the_user_counts_as_still_after_a_pause() {
        assert!(!is_quiet(0));
        assert!(!is_quiet(QUIET_MS - 1));
        assert!(is_quiet(QUIET_MS));
        assert!(is_quiet(60_000));
    }
}
