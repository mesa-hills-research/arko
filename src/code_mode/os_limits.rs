//! The limits the code runner sets on itself with the operating system,
//! before it reads any code. They back up the engine's own: a fault in the
//! engine, or a value too large for the heap's count, ends the runner and
//! never the machine or Arko.
//!
//! | | Linux | macOS | Windows |
//! |---|---|---|---|
//! | Memory | `RLIMIT_DATA`, and `RLIMIT_AS` above what it starts with | The same | The Job Object's process memory |
//! | Processor time | `RLIMIT_CPU` | `RLIMIT_CPU` | The Job Object's user time |
//! | Core dumps, file writes | `RLIMIT_CORE`, `RLIMIT_FSIZE` at 0 | The same | No error dialog, one process |
//! | Out of memory | `oom_score_adj` 1000, so it goes first | | |

use super::protocol::Limits;

/// Sets `limits`' backstops on this process. An error says which couldn't
/// be set, and the runner then runs no code.
pub fn apply(limits: &Limits) -> Result<(), String> {
    imp::apply(limits)
}

#[cfg(unix)]
mod imp {
    use super::Limits;

    /// The type `getrlimit` and `setrlimit` take a resource as, which
    /// differs between C libraries.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    type Resource = libc::__rlimit_resource_t;
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    type Resource = libc::c_int;

    pub fn apply(limits: &Limits) -> Result<(), String> {
        // The address space counts every mapping, shared ones too, from the
        // size the program starts with, which differs between systems.
        let start = address_space()?;
        set(
            libc::RLIMIT_AS,
            "RLIMIT_AS",
            start.saturating_add(limits.os_address_space_bytes),
            0,
        )?;
        set(libc::RLIMIT_DATA, "RLIMIT_DATA", limits.os_memory_bytes, 0)?;
        // The processor time ends the runner with SIGXCPU, and SIGKILL a
        // little later.
        set(libc::RLIMIT_CPU, "RLIMIT_CPU", limits.cpu_seconds, 5)?;
        set(libc::RLIMIT_CORE, "RLIMIT_CORE", 0, 0)?;
        set(libc::RLIMIT_FSIZE, "RLIMIT_FSIZE", 0, 0)?;
        #[cfg(target_os = "linux")]
        std::fs::write("/proc/self/oom_score_adj", "1000")
            .map_err(|error| format!("couldn't raise oom_score_adj ({error})"))?;
        Ok(())
    }

    /// This process's address space, in bytes.
    #[cfg(target_os = "linux")]
    fn address_space() -> Result<u64, String> {
        let statm = std::fs::read_to_string("/proc/self/statm")
            .map_err(|error| format!("couldn't read its address space ({error})"))?;
        let pages: u64 = statm
            .split_whitespace()
            .next()
            .and_then(|pages| pages.parse().ok())
            .ok_or("couldn't read its address space")?;
        // SAFETY: `sysconf` reads a constant of the system.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Ok(pages * u64::try_from(page).map_err(|_| "no page size")?)
    }

    #[cfg(target_os = "macos")]
    fn address_space() -> Result<u64, String> {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: `info` is the structure the flavor fills in, with its size.
        let read = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if read != size {
            return Err("couldn't read its address space".into());
        }
        Ok(info.pti_virtual_size)
    }

    /// (Elsewhere the address space is left as it is.)
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn address_space() -> Result<u64, String> {
        Ok(u64::MAX / 2)
    }

    /// Lowers `resource` to `soft`, and its hard limit to `soft + room`,
    /// neither higher than it was.
    fn set(resource: Resource, name: &str, soft: u64, room: u64) -> Result<(), String> {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `current` is a valid `rlimit` for the call to fill in.
        if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
            return Err(format!(
                "couldn't read {name} ({})",
                std::io::Error::last_os_error()
            ));
        }
        let hard = (soft.saturating_add(room) as libc::rlim_t).min(current.rlim_max);
        let wanted = libc::rlimit {
            rlim_cur: (soft as libc::rlim_t).min(hard),
            rlim_max: hard,
        };
        // SAFETY: `wanted` is a valid `rlimit`.
        if unsafe { libc::setrlimit(resource, &wanted) } != 0 {
            return Err(format!(
                "couldn't set {name} ({})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use super::Limits;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// A Job Object of this process alone, with its limits. Its handle stays
    /// open as long as the process runs.
    pub fn apply(limits: &Limits) -> Result<(), String> {
        let error = |what: &str| format!("couldn't {what} ({})", std::io::Error::last_os_error());
        // SAFETY: no attributes and no name, as the call allows.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(error("make a Job Object"));
        }
        // SAFETY: the structure is plain data, all zeroes until set.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_PROCESS_TIME
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // In units of 100 nanoseconds.
        info.BasicLimitInformation.PerProcessUserTimeLimit =
            i64::try_from(limits.cpu_seconds.saturating_mul(10_000_000)).unwrap_or(i64::MAX);
        info.BasicLimitInformation.ActiveProcessLimit = 1;
        info.ProcessMemoryLimit = usize::try_from(limits.os_memory_bytes).unwrap_or(usize::MAX);
        // SAFETY: `info` is the structure the class names, with its size.
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            return Err(error("set the Job Object's limits"));
        }
        // SAFETY: the job just made, and this process's pseudo handle.
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == 0 {
            return Err(error("put the runner in its Job Object"));
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::Limits;

    pub fn apply(_: &Limits) -> Result<(), String> {
        Err("this system has no limits the runner can set".into())
    }
}
