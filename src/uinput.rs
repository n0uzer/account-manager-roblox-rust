//! One virtual keyboard, for anti-AFK on Linux.
//!
//! Wayland forbids sending input to a window that is not focused, but it does
//! not forbid the kernel's `uinput` device: a program that can write
//! `/dev/uinput` can create a keyboard, and the compositor routes its events to
//! the focused window exactly as if a real keyboard had sent them. This module
//! owns that keyboard: one device, created on first use and removed when the
//! manager exits (the kernel drops it with the last file descriptor).
//!
//! No injection into the game, no access to its memory: a plain F15 tap, a key
//! no game binds to anything.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const DEVICE: &str = "/dev/uinput";
const KEY_F15: u16 = 186;
const EV_KEY: u16 = 1;
const EV_SYN: u16 = 0;
const SYN_REPORT: u16 = 0;

const UI_DEV_CREATE: u64 = 0x5501;
const UI_DEV_SETUP: u64 = 0x405c_5503;
const UI_SET_EVBIT: u64 = 0x4004_5564;
const UI_SET_KEYBIT: u64 = 0x4004_5565;

/// `struct input_id`.
#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_setup`: the name and the id of the device to create.
#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// `struct input_event`: a timestamp, a type, a code and a value. Both time
/// fields are 64-bit on every Linux ABI this builds for.
#[repr(C)]
struct InputEvent {
    sec: i64,
    usec: i64,
    kind: u16,
    code: u16,
    value: i32,
}

extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// Whether the manager may create a keyboard at all.
///
/// Cached: the UI asks on every frame, and permission does not change while
/// the manager runs.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| OpenOptions::new().write(true).open(DEVICE).is_ok())
}

/// Tap F15 on the virtual keyboard. Returns whether the kernel took every
/// event.
pub fn tap_f15() -> bool {
    with_device(|device| {
        let down = InputEvent {
            sec: 0,
            usec: 0,
            kind: EV_KEY,
            code: KEY_F15,
            value: 1,
        };
        let up = InputEvent { value: 0, ..down };
        let sync = InputEvent {
            sec: 0,
            usec: 0,
            kind: EV_SYN,
            code: SYN_REPORT,
            value: 0,
        };

        // A real key is held for a moment; some engines drop a press and a
        // release that arrive in the same frame.
        send(device, &down) && send(device, &sync) && {
            std::thread::sleep(Duration::from_millis(25));
            send(device, &up) && send(device, &sync)
        }
    })
    .unwrap_or(false)
}

fn with_device<T>(action: impl FnOnce(&mut File) -> T) -> Option<T> {
    static SLOT: OnceLock<Mutex<Option<File>>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| Mutex::new(open_device().ok()));
    let mut device = slot.lock().ok()?;
    if device.is_none() {
        // The first open may have failed because permission was granted later.
        *device = open_device().ok();
    }
    Some(action(device.as_mut()?))
}

fn open_device() -> Result<File, String> {
    let file = OpenOptions::new()
        .write(true)
        .open(DEVICE)
        .map_err(|err| format!("не открыть {DEVICE}: {err}"))?;
    let fd = file.as_raw_fd();

    let mut name = [0u8; 80];
    let label = b"rbm-anti-afk";
    name[..label.len()].copy_from_slice(label);
    let setup = UinputSetup {
        id: InputId {
            bustype: 0x0003, // BUS_USB
            vendor: 0x1234,
            product: 0x5678,
            version: 1,
        },
        name,
        ff_effects_max: 0,
    };

    // SAFETY: the descriptor is open, the request numbers are the kernel's,
    // and `setup` is a plain C struct that lives for the call.
    unsafe {
        if ioctl(fd, UI_SET_EVBIT, EV_KEY as i32) < 0
            || ioctl(fd, UI_SET_KEYBIT, KEY_F15 as i32) < 0
            || ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) < 0
            || ioctl(fd, UI_DEV_CREATE) < 0
        {
            return Err("не создать виртуальную клавиатуру".to_string());
        }
    }

    // The device node appears asynchronously; events sent before the
    // compositor has seen it are simply lost.
    std::thread::sleep(Duration::from_millis(150));
    Ok(file)
}

fn send(device: &mut File, event: &InputEvent) -> bool {
    // SAFETY: `InputEvent` is plain integers with no padding, and the slice
    // only covers that one value.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            event as *const InputEvent as *const u8,
            std::mem::size_of::<InputEvent>(),
        )
    };
    device.write_all(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_layout_is_what_the_kernel_expects() {
        // 64-bit timeval plus type, code and value: the ABI is fixed.
        assert_eq!(std::mem::size_of::<InputEvent>(), 24);
        assert_eq!(std::mem::size_of::<UinputSetup>(), 92);
        assert_eq!(std::mem::size_of::<InputId>(), 8);
    }

    #[test]
    fn the_key_is_f15() {
        assert_eq!(KEY_F15, 186);
    }

    #[test]
    fn asking_for_availability_never_panics() {
        let _ = available();
    }

    #[test]
    #[ignore = "creates a real /dev/uinput device and taps F15"]
    fn smoke_tap_f15() {
        assert!(available(), "/dev/uinput is not writable");
        assert!(tap_f15(), "the kernel refused the events");
    }
}
