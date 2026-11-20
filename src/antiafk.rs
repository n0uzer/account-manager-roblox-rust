//! Optional anti-AFK: a tiny input event now and then, so a background window is
//! not kicked for sitting idle.
//!
//! Roblox removes a player who has sent no input for about 20 minutes. Every
//! `interval` minutes each client window is sent two harmless messages:
//!
//! * a 1-pixel mouse move and back, posted to the window itself, and
//! * a press of F15, a key no game binds to anything.
//!
//! The messages are *posted* to the window (`PostMessageW`), so nothing here takes
//! focus, moves the real cursor or types into whatever window is in front. The
//! client is not injected into and its memory is not touched. Off by default.
//!
//! Limits worth knowing:
//! * a client running as administrator ignores messages from a normal-rights
//!   manager (Windows blocks that on purpose);
//! * whether the client counts posted messages as "activity" depends on the client
//!   version, so the effect has to be checked on the real client;
//! * it is still automation: some games and Roblox's rules treat that as a
//!   violation. Use at your own risk, see the README.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use crate::window;

pub const MIN_MINUTES: u32 = 1;
/// Roblox kicks at 20 minutes, so a longer interval could not help.
pub const MAX_MINUTES: u32 = 19;
pub const DEFAULT_MINUTES: u32 = 10;

/// How often the worker looks at the clock.
const TICK: Duration = Duration::from_secs(5);

/// Virtual-key code and scan code of F15.
#[cfg_attr(not(windows), allow(dead_code))]
const VK_F15: usize = 0x7E;
#[cfg_attr(not(windows), allow(dead_code))]
const SCAN_F15: u32 = 0x64;

static ENABLED: AtomicBool = AtomicBool::new(false);
static MINUTES: AtomicU32 = AtomicU32::new(DEFAULT_MINUTES);
static STARTED: Once = Once::new();

pub fn clamp_minutes(minutes: u32) -> u32 {
    minutes.clamp(MIN_MINUTES, MAX_MINUTES)
}

/// Whether enough time has passed since the last pulse.
fn is_due(since_last: Duration, minutes: u32) -> bool {
    since_last >= Duration::from_secs(u64::from(clamp_minutes(minutes)) * 60)
}

/// Apply the settings. Starts the worker the first time anti-AFK is turned on.
pub fn configure(enabled: bool, minutes: u32) {
    MINUTES.store(clamp_minutes(minutes), Ordering::Relaxed);
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
                        pulse();
                        last = Instant::now();
                    }
                }
            });
        // A failed thread only means no anti-AFK.
        let _ = spawned;
    });
}

/// Send one round of input to every client window. Returns how many windows got
/// it.
pub fn pulse() -> usize {
    window::game_window_handles()
        .into_iter()
        .filter(|hwnd| win::nudge(*hwnd))
        .count()
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
        fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
        fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    }

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
}

#[cfg(not(windows))]
mod win {
    pub(super) fn nudge(_handle: isize) -> bool {
        false
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
        assert_eq!(pulse(), 0);
    }

    #[test]
    fn settings_are_clamped_when_applied() {
        configure(false, 500);
        assert_eq!(MINUTES.load(Ordering::Relaxed), MAX_MINUTES);
        configure(false, DEFAULT_MINUTES);
        assert!(!ENABLED.load(Ordering::Relaxed));
    }
}
