//! Spawning `rg`, the editor, and Explorer without flashing a console window,
//! and making sure no `rg` outlives the app.

use std::io;
use std::process::{Child, Command};

/// A `Command` that won't open a console window when the app itself is a
/// GUI-subsystem process (every release build). Without `CREATE_NO_WINDOW`
/// each ripgrep spawn would flash a terminal over the user's work.
pub fn no_window_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Spawns `cmd` and assigns the child to a process-wide job object created
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The job handle is only closed
/// by the OS when this process exits — however it exits, crash and Task
/// Manager included — which then kills every `rg` still running. Normal
/// cancellation still kills children explicitly; this is the backstop.
///
/// If the job can't be created or the assignment fails, the child is kept
/// running unbound rather than failing the search.
pub fn spawn_bound_to_job(cmd: &mut Command) -> io::Result<Child> {
    let child = cmd.spawn()?;
    #[cfg(windows)]
    job::assign(&child);
    Ok(child)
}

#[cfg(windows)]
mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The raw handle as an integer so it can sit in a `static`. It is never
    /// closed: its lifetime is the process's lifetime, by design.
    static JOB: OnceLock<Option<usize>> = OnceLock::new();

    fn job() -> Option<HANDLE> {
        let handle = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                return None;
            }
            Some(job as usize)
        });
        handle.map(|h| h as HANDLE)
    }

    pub fn assign(child: &Child) {
        let Some(job) = job() else { return };
        let process = child.as_raw_handle() as HANDLE;
        // SAFETY: both handles are valid for the duration of the call; the
        // child's handle is owned by `child`, the job's by the static above.
        unsafe {
            AssignProcessToJobObject(job, process);
        }
    }
}
