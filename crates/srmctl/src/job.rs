//! Running a child inside a container the OS accounts for.
//!
//! On Windows that is a Job Object, and it does three things nothing else can:
//!
//! - Its priority limit applies to every process in it, for its whole life. Setting a
//!   priority on the child races the processes it starts in its first milliseconds.
//! - It knows exactly which processes are in it, including ones whose parent has
//!   already exited, which breaks any walk of the parent/child tree.
//! - Its CPU total includes processes that have exited. A build's short-lived
//!   compilers are most of its work, and a sampler misses them.
//!
//! Once the child is running the job isn't kill-on-close. Build tools leave servers
//! behind on purpose (`sccache`, `mspdbsrv`), and ending the wrapper shouldn't end those.
//! It is until then: the child starts suspended, and a wrapper killed before resuming it
//! would leave a process frozen forever, holding every handle it inherited.

use std::process::ExitStatus;
use tokio::process::{Child, Command};

pub struct Usage {
    pub pids: Vec<u32>,
    pub cpu_seconds: f64,
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, BELOW_NORMAL_PRIORITY_CLASS, CREATE_SUSPENDED,
        THREAD_SUSPEND_RESUME,
    };

    /// More than any build has running at once. A list that doesn't fit is reported
    /// truncated rather than failed: accounting is still useful without the last few.
    const MAX_LISTED: usize = 4096;

    pub struct Job {
        handle: HANDLE,
    }

    // SAFETY: a job handle is a process-wide kernel object reference; the calls made
    // on it here are safe from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: owned by this struct and closed only here.
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// Replaces every limit, so it states all of them.
    fn set_limits(job: &Job, low: bool, kill_on_close: bool) -> std::io::Result<()> {
        // SAFETY: the struct is plain data, for which all-zero is a valid "no limits".
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        // Breakaway stays forbidden. Cygwin and MSYS start every child outside the job
        // whenever a job permits it, so allowing it would lose everything run through
        // `bash -c`: its priority limit, its accounting, the lot.
        if low {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PRIORITY_CLASS;
            limits.BasicLimitInformation.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS;
        }
        if kill_on_close {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        // SAFETY: the pointer and size describe `limits`, which outlives the call.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn create(low: bool) -> std::io::Result<Job> {
        // SAFETY: both arguments are optional and null asks for an anonymous job with
        // default security. The handle is checked before use and owned by `Job`.
        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job { handle };
        set_limits(&job, low, true)?;
        Ok(job)
    }

    /// Resume a process created suspended. `std` keeps the thread handle to itself,
    /// so find the thread by its owner.
    fn resume(pid: u32) -> std::io::Result<()> {
        // SAFETY: a thread snapshot takes no process argument; the handle is checked
        // and closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: plain data; `dwSize` is set as the API requires.
        let mut entry: THREADENTRY32 = unsafe { zeroed() };
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        // SAFETY: `entry` is valid and sized; iteration stops when the API says so.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: the handle is checked, used once, and closed.
                unsafe {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        if ResumeThread(thread) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(thread);
                    }
                }
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        // SAFETY: from CreateToolhelp32Snapshot and not used again.
        unsafe { CloseHandle(snapshot) };
        if resumed == 0 {
            return Err(std::io::Error::other("found no thread to resume"));
        }
        Ok(())
    }

    impl Job {
        pub fn usage(&self) -> Option<Usage> {
            // SAFETY: the out-pointer and size describe a live, plain-data struct.
            let cpu_seconds = unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
                let ok = QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    (&raw mut info).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                );
                if ok == 0 {
                    return None;
                }
                // 100 ns ticks.
                (info.TotalUserTime + info.TotalKernelTime) as f64 / 1e7
            };

            // The struct ends in a flexible array, so build a buffer big enough for it.
            let bytes = size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()
                + (MAX_LISTED - 1) * size_of::<usize>();
            let mut buffer = vec![0usize; bytes / size_of::<usize>()];
            let list = buffer
                .as_mut_ptr()
                .cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            // SAFETY: `buffer` is usize-aligned, as the struct needs, and at least
            // `bytes` long. An overflow reports ERROR_MORE_DATA and fills what fits.
            let pids = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicProcessIdList,
                    list.cast(),
                    bytes as u32,
                    null_mut(),
                );
                let count = (*list).NumberOfProcessIdsInList as usize;
                std::slice::from_raw_parts(
                    (&raw const (*list).ProcessIdList).cast::<usize>(),
                    count.min(MAX_LISTED),
                )
                .iter()
                .map(|pid| *pid as u32)
                .collect()
            };
            Some(Usage { pids, cpu_seconds })
        }
    }

    pub fn spawn(command: &mut Command, low: bool) -> std::io::Result<(Child, Option<Job>)> {
        let job = match create(low) {
            Ok(job) => Some(job),
            Err(e) => {
                eprintln!("srmctl: no job object ({e}); running without exact accounting");
                None
            }
        };
        if job.is_none() {
            return command.spawn().map(|child| (child, None));
        }

        command.creation_flags(CREATE_SUSPENDED);
        let child = command.spawn()?;
        let pid = child.id().unwrap_or(0);
        let job = job.filter(|job| {
            let process = child.raw_handle().unwrap_or(null_mut());
            // SAFETY: both handles are live: the job is owned above, and the process
            // can't have exited because it hasn't started.
            let ok = unsafe { AssignProcessToJobObject(job.handle, process) };
            if ok == 0 {
                eprintln!(
                    "srmctl: couldn't join job ({}); running without exact accounting",
                    std::io::Error::last_os_error()
                );
            }
            ok != 0
        });
        resume(pid)?;
        if let Some(job) = &job {
            if let Err(e) = set_limits(job, low, false) {
                eprintln!("srmctl: anything left running will end with this command ({e})");
            }
        }
        Ok((child, job))
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    /// The kernel's process groups give no totals, so on Linux the daemon's tree walk
    /// does the accounting.
    pub struct Job;

    impl Job {
        pub fn usage(&self) -> Option<Usage> {
            None
        }
    }

    pub fn spawn(command: &mut Command, low: bool) -> std::io::Result<(Child, Option<Job>)> {
        if low {
            // SAFETY: `nice` is async-signal-safe, so it's allowed between fork and
            // exec. Failure leaves the priority unchanged, which is harmless.
            unsafe {
                command.pre_exec(|| {
                    libc::nice(10);
                    Ok(())
                });
            }
        }
        command.spawn().map(|child| (child, None))
    }
}

pub use imp::{spawn, Job};

/// A child's exit status as a process exit code.
pub fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
