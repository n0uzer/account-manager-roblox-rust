//! Windows DPAPI: binds data to the current Windows user account.
//!
//! Implemented with a direct `crypt32.dll` FFI, no extra crates.
//! On other platforms every function fails, the app is Windows-only anyway.

#![allow(non_snake_case)]

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    #[derive(Default)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;

        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    /// Never show a DPAPI prompt: we run from the GUI, not a script host.
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x01;

    fn make_blob(data: &[u8]) -> DataBlob {
        DataBlob {
            cb_data: data.len() as u32,
            pb_data: data.as_ptr() as *mut u8,
        }
    }

    fn take_output(output: DataBlob) -> Vec<u8> {
        if output.pb_data.is_null() || output.cb_data == 0 {
            return Vec::new();
        }
        // SAFETY: crypt32 allocated the buffer and reported its length.
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec() };
        // SAFETY: the buffer came from LocalAlloc inside crypt32.
        unsafe {
            LocalFree(output.pb_data as *mut c_void);
        }
        bytes
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        if data.is_empty() {
            return Err("DPAPI: пустые данные".into());
        }
        let input = make_blob(data);
        let mut output = DataBlob::default();

        // SAFETY: both blobs are valid for the duration of the call.
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };

        if ok == 0 {
            return Err(format!(
                "DPAPI: не удалось зашифровать ({})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(take_output(output))
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        if data.is_empty() {
            return Err("DPAPI: пустые данные".into());
        }
        let input = make_blob(data);
        let mut output = DataBlob::default();

        // SAFETY: both blobs are valid for the duration of the call.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };

        if ok == 0 {
            return Err("DPAPI: файл привязан к другому пользователю Windows или повреждён".into());
        }
        Ok(take_output(output))
    }
}

#[cfg(windows)]
pub use win::{protect, unprotect};

#[cfg(not(windows))]
pub fn protect(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI доступен только на Windows".into())
}

#[cfg(not(windows))]
pub fn unprotect(_data: &[u8]) -> Result<Vec<u8>, String> {
    Err("DPAPI доступен только на Windows".into())
}
