//! Ties child processes to this process's lifetime.
//!
//! On Windows: a Job Object with KILL_ON_JOB_CLOSE: every child assigned to it dies when this process does,
//! including a hard kill from Task Manager or a crash (design.md §2.7). Without it, ffmpeg / the capture engine
//! / mpv can be left running with no window and no way to stop them except Task Manager.
//!
//! Elsewhere there is no such object: the job remembers its children and kills them when it is dropped, which
//! covers every orderly exit but not a crash of this process.
//!
//! Everything is best effort: on a platform or a sandbox where jobs are unavailable the calls fail silently and
//! the app keeps working (the normal Stop path still kills the process tree).

use std::process::Child;

#[derive(Debug)]
pub struct ProcessJob {
    inner: imp::Job,
}

impl ProcessJob {
    /// Never fails: when the job cannot be created, [`is_available`](Self::is_available) is false and
    /// [`assign`](Self::assign) does nothing.
    pub fn new() -> ProcessJob {
        ProcessJob { inner: imp::Job::new() }
    }

    /// True when the job exists; false means children are not tied to this process's lifetime.
    pub fn is_available(&self) -> bool {
        self.inner.is_available()
    }

    /// Ties a started child process to this job. Returns false when it could not be assigned (already exited,
    /// or the process is in another job that disallows nesting).
    pub fn assign(&self, child: &Child) -> bool {
        self.inner.assign(child)
    }

    /// The child with this process id was waited for and is gone: forget it. On Windows the job tracks its
    /// members itself and this does nothing; elsewhere it stops a later drop from signalling a process id that
    /// may by then belong to somebody else.
    pub fn release(&self, pid: u32) {
        self.inner.release(pid);
    }
}

impl Default for ProcessJob {
    fn default() -> Self {
        ProcessJob::new()
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    #[derive(Debug)]
    pub struct Job {
        handle: Option<HANDLE>,
    }

    // SAFETY: a job object handle is a kernel handle; the calls made on it here are thread-safe.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn new() -> Job {
            Job { handle: create() }
        }

        pub fn is_available(&self) -> bool {
            self.handle.is_some()
        }

        pub fn assign(&self, child: &Child) -> bool {
            let Some(job) = self.handle else { return false };
            // SAFETY: both handles are valid for the duration of the call (`child` owns its process handle).
            unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }.is_ok()
        }

        pub fn release(&self, _pid: u32) {}
    }

    impl Drop for Job {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                // Closing the last handle is what kills the children still in the job.
                // SAFETY: the handle was created by `create` and is closed exactly once.
                let _ = unsafe { CloseHandle(handle) };
            }
        }
    }

    fn create() -> Option<HANDLE> {
        // SAFETY: plain Win32 calls; `info` is a properly sized, zero-initialized structure that outlives the
        // call, and the handle is closed again when the limit cannot be set.
        unsafe {
            let handle = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set.is_err() {
                let _ = CloseHandle(handle);
                return None;
            }
            Some(handle)
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::process::Child;
    use std::sync::Mutex;

    #[derive(Debug)]
    pub struct Job {
        children: Mutex<Vec<u32>>,
    }

    impl Job {
        pub fn new() -> Job {
            Job { children: Mutex::new(Vec::new()) }
        }

        pub fn is_available(&self) -> bool {
            true
        }

        pub fn assign(&self, child: &Child) -> bool {
            let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
            if !children.contains(&child.id()) {
                children.push(child.id());
            }
            true
        }

        pub fn release(&self, pid: u32) {
            self.children.lock().unwrap_or_else(|e| e.into_inner()).retain(|&p| p != pid);
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            let children = std::mem::take(self.children.get_mut().unwrap_or_else(|e| e.into_inner()));
            for pid in children {
                // SAFETY: `kill` has no memory-safety preconditions. The id is a child of ours that was not
                // released, i.e. not reaped, so it still names that child (or its zombie). The negative id is
                // the child's process group, when it leads one (LineProcess children do): its own children go
                // with it. A child that leads no group makes that call fail harmlessly.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}
