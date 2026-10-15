//! The parent watch (`--parent-pid`).

use crate::platform::ProcessWatch;

/// Tells whether a process still exists.
///
/// A process id is not a handle: it can be given to another program once its owner is gone. When the
/// watched process is our own parent - the one case the engine uses - that cannot fool us, because the
/// moment the parent ends the kernel hands us to another parent and `getppid` changes for good. For any
/// other process the answer comes from `kill(pid, 0)`, which sends nothing and only reports existence.
pub(crate) struct PidWatch {
    pid: libc::pid_t,
    is_parent: bool,
}

impl PidWatch {
    /// `None` for an id that cannot name a process (0 and negative values address process groups).
    pub(crate) fn open(pid: u32) -> Option<Self> {
        let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: plain call without arguments; it cannot fail.
        let is_parent = unsafe { libc::getppid() } == pid;
        Some(Self { pid, is_parent })
    }
}

impl ProcessWatch for PidWatch {
    fn is_gone(&self) -> bool {
        // SAFETY: plain call without arguments; it cannot fail.
        if self.is_parent && unsafe { libc::getppid() } != self.pid {
            return true;
        }
        // SAFETY: signal 0 performs the existence and permission checks only; nothing is delivered. `pid`
        // is positive, so this never addresses a process group.
        let result = unsafe { libc::kill(self.pid, 0) };
        // EPERM: it exists but belongs to someone else.
        result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_and_its_parent_are_alive() {
        let me = PidWatch::open(std::process::id()).unwrap();
        assert!(!me.is_gone());
        // SAFETY: plain call without arguments.
        let parent = unsafe { libc::getppid() };
        let watch = PidWatch::open(parent as u32).unwrap();
        assert!(watch.is_parent);
        assert!(!watch.is_gone());
    }

    #[test]
    fn a_child_that_exited_and_was_reaped_is_gone() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let watch = PidWatch::open(child.id()).unwrap();
        child.wait().unwrap();
        assert!(watch.is_gone());
    }

    #[test]
    fn ids_that_name_no_process_are_refused() {
        assert!(PidWatch::open(0).is_none());
        assert!(PidWatch::open(u32::MAX).is_none());
    }
}
