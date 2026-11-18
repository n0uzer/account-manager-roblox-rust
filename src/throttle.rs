//! Per-window CPU limit through a Windows job object.
//!
//! The FPS knob in `ClientAppSettings.json` is a fast flag, and current clients
//! apply only the flags from Roblox's allowlist. `DFIntTaskSchedulerTargetFps`
//! is not on that list, so the client drops it and the cap does nothing.
//!
//! The OS can still hold a client back: a job object with
//! `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` and a hard cap limits how much CPU
//! time the process (and everything it starts) gets. Less CPU per window means
//! fewer frames and less heat, and the client cannot opt out of it.
//!
//! One job is created per percentage value and kept open for the whole run:
//! closing the last handle would drop the limit.

use std::sync::{Mutex, MutexGuard};

/// Jobs we created, keyed by their CPU percentage. Reusing one job per value
/// also keeps the handles alive, which the limit needs.
static JOBS: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

/// Cap `pid` to `percent` of the machine's CPU.
///
/// `percent == 0` means "no limit" and returns an empty note. A note like
/// `CPU ≤ 25%` is meant for the status bar.
pub fn limit(pid: u32, percent: u32) -> Result<String, String> {
    if percent == 0 {
        return Ok(String::new());
    }
    let percent = budget(percent);
    let job = job_for(percent)?;
    win::assign(job, pid).map_err(|code| {
        format!("лимит CPU не применён: Windows error {code}. Клиент уже в другом job-объекте?")
    })?;
    Ok(format!("CPU ≤ {percent}%"))
}

/// Valid CPU budget for a user-entered percentage.
fn budget(percent: u32) -> u32 {
    percent.clamp(1, 100)
}

/// Job handle for this percentage, created on first use.
fn job_for(percent: u32) -> Result<usize, String> {
    let mut jobs = lock();
    if let Some((_, handle)) = jobs.iter().find(|(value, _)| *value == percent) {
        return Ok(*handle);
    }
    match win::cpu_job(percent) {
        Ok(handle) => {
            jobs.push((percent, handle));
            Ok(handle)
        }
        Err(code) => Err(format!(
            "лимит CPU не применён: не удалось создать job-объект (Windows error {code})"
        )),
    }
}

fn lock() -> MutexGuard<'static, Vec<(u32, usize)>> {
    JOBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Logical processors on this machine. The budget is a share of the whole CPU,
/// so the UI has to spell out how many cores a percentage really is.
pub fn logical_cpus() -> u32 {
    win::logical_cpus().max(1)
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::ptr;

    type Handle = *mut c_void;

    /// `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION.ControlFlags`: turn the limit on.
    const ENABLE: u32 = 0x1;
    /// A hard cap, not a soft hint: threads are throttled when the budget is gone.
    const HARD_CAP: u32 = 0x4;
    /// `JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation`.
    const CPU_RATE_CONTROL_INFORMATION: i32 = 15;
    const PROCESS_SET_QUOTA: u32 = 0x0100;
    const PROCESS_TERMINATE: u32 = 0x0001;

    /// `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`: flags plus a union of
    /// `CpuRate`/`Weight`/`MinRate`+`MaxRate`. Only `CpuRate` is used, and a
    /// `u32` covers the whole union.
    #[repr(C)]
    struct CpuRateControl {
        flags: u32,
        rate: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, len: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetSystemInfo(info: *mut SystemInfo);
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    /// `SYSTEM_INFO`, only the field we need spelled out; the rest keeps the
    /// layout so the call is safe.
    #[repr(C)]
    struct SystemInfo {
        architecture: u16,
        reserved: u16,
        page_size: u32,
        min_app: *mut c_void,
        max_app: *mut c_void,
        affinity_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    pub fn logical_cpus() -> u32 {
        let mut info = SystemInfo {
            architecture: 0,
            reserved: 0,
            page_size: 0,
            min_app: ptr::null_mut(),
            max_app: ptr::null_mut(),
            affinity_mask: 0,
            number_of_processors: 0,
            processor_type: 0,
            allocation_granularity: 0,
            processor_level: 0,
            processor_revision: 0,
        };
        unsafe { GetSystemInfo(&mut info) };
        info.number_of_processors
    }

    /// Create a job whose CPU budget is `percent` of the whole machine.
    pub fn cpu_job(percent: u32) -> Result<usize, u32> {
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(unsafe { GetLastError() });
        }

        let info = CpuRateControl {
            flags: ENABLE | HARD_CAP,
            // Hundredths of a percent: 10000 is the whole machine.
            rate: percent * 100,
        };
        let ok = unsafe {
            SetInformationJobObject(
                job,
                CPU_RATE_CONTROL_INFORMATION,
                (&info as *const CpuRateControl).cast(),
                std::mem::size_of::<CpuRateControl>() as u32,
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            unsafe { CloseHandle(job) };
            return Err(code);
        }
        Ok(job as usize)
    }

    /// Put `pid` under the job. Needs `PROCESS_SET_QUOTA`, hence `OpenProcess`.
    pub fn assign(job: usize, pid: u32) -> Result<(), u32> {
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(unsafe { GetLastError() });
        }
        let ok = unsafe { AssignProcessToJobObject(job as Handle, process) };
        let code = unsafe { GetLastError() };
        unsafe { CloseHandle(process) };
        if ok == 0 {
            Err(code)
        } else {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub fn cpu_job(_percent: u32) -> Result<usize, u32> {
        Ok(0)
    }

    pub fn assign(_job: usize, _pid: u32) -> Result<(), u32> {
        Ok(())
    }

    pub fn logical_cpus() -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_means_off() {
        let note = limit(0, 0).expect("zero is not an error");
        assert!(note.is_empty());
    }

    #[test]
    fn one_job_per_percentage() {
        let first = job_for(70).expect("job");
        let second = job_for(70).expect("job");
        let other = job_for(80).expect("job");
        assert_eq!(first, second);
        assert_ne!(first, other);
    }

    #[test]
    fn machine_has_at_least_one_processor() {
        assert!(logical_cpus() >= 1);
        // Sanity: a real desktop reports something believable.
        assert!(logical_cpus() <= 1024);
    }

    #[test]
    fn percentages_are_clamped_to_the_valid_range() {
        // Both ends have to map onto a real budget instead of failing.
        assert_eq!(budget(0), 1);
        assert_eq!(budget(250), 100);
        assert_eq!(budget(25), 25);
        assert_eq!(job_for(budget(250)), job_for(100));
    }
}
