//! Auto-kill child processes when Sirin exits.
//!
//! ## Problem
//!
//! Rust's [`std::process::Child`] does **not** kill the underlying OS
//! process when dropped (well-known footgun).  When `Command::output()` is
//! interrupted by a thread panic, when Sirin crashes, or when the user
//! force-kills `sirin.exe`, every spawned child (claude, node, git, …)
//! becomes an orphan and keeps running until it exits naturally.
//!
//! Observed 2026-04-19: 12 orphaned `claude.exe` processes from morning
//! crashes, several burning 1-2 h of CPU time — invisible to new Sirin
//! instances and never reaped until the user logged out.
//!
//! ## Solution (Windows)
//!
//! Create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assign
//! the current process to it at startup.  When Sirin exits for **any**
//! reason — graceful shutdown, panic, SIGSEGV, `taskkill /F`, even closing
//! the GUI window — the kernel closes the job handle, which terminates
//! every process in the job (including all transitive children).
//!
//! ## Solution (Unix)
//!
//! No-op for now.  Unix already has `prctl(PR_SET_PDEATHSIG)` for child→
//! parent death-watch, but it's per-child and not as comprehensive as
//! Job Objects.  If we ever see orphan claude on macOS/Linux we can add a
//! `setsid` + `killpg` pattern here.

/// Install a process-tree kill switch.  Call once at startup (before any
/// subprocess is spawned).  No-op outside Windows.  Logs but does not panic
/// on failure — the worst-case fallback is the previous orphan-leak behavior.
pub fn install() {
    #[cfg(windows)]
    if let Err(e) = install_windows_job() {
        tracing::warn!(target: "sirin",
            "[process_group] Failed to install kill-on-close job: {e}. \
             Child processes (claude, node, git) may orphan if Sirin crashes.");
    } else {
        tracing::info!(target: "sirin",
            "[process_group] Windows Job Object installed — children will be killed on Sirin exit");
    }

    #[cfg(not(windows))]
    {
        // No-op on Unix.  Add prctl(PDEATHSIG) per-child if needed.
    }
}

#[cfg(windows)]
fn install_windows_job() -> Result<(), String> {
    use std::mem;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        // Create an unnamed job object.
        let job: HANDLE = CreateJobObjectW(None, None)
            .map_err(|e| format!("CreateJobObjectW: {e}"))?;

        // Configure: kill all members when job handle closes.
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };

        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("SetInformationJobObject: {e}"))?;

        // Assign current process — children spawned afterwards inherit
        // job membership automatically (this is the default behavior on
        // modern Windows; older Windows required explicit per-child assignment).
        AssignProcessToJobObject(job, GetCurrentProcess())
            .map_err(|e| format!("AssignProcessToJobObject: {e}"))?;

        // Intentionally do NOT call CloseHandle on `job`.  The kernel keeps
        // the job alive while *any* handle is open; we want the last close
        // to be the kernel tearing down our process at exit, which fires
        // KILL_ON_JOB_CLOSE.  HANDLE is `Copy` (a wrapper around *mut c_void),
        // so simply discarding the binding leaks it for the process lifetime.
        let _ = job;
    }

    Ok(())
}
