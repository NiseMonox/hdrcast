//! A child process with redirected stdout/stderr delivered as line callbacks on reader threads, an exit callback
//! that fires after the output streams drained, and a `stop()` that kills the whole process tree. Stdout can
//! alternatively be handed over raw (ffmpeg's MPEG-TS, ffmpeg | mpv piping) and stdin can be redirected.
//!
//! The child is started from an argv, never from a command-line string: `std::process::Command` does the
//! quoting CreateProcess needs. [`LineProcess::command_line`] is only what goes into the log.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::arg_quote;
use super::process_job::ProcessJob;
use super::secret_redactor;

/// How long [`LineProcess::stop`] waits for the killed process to be gone.
const STOP_WAIT: Duration = Duration::from_secs(3);

type LineCallback = Box<dyn FnMut(&str) + Send>;
type ExitCallback = Box<dyn FnOnce(i32) + Send>;

pub struct LineProcess {
    program: OsString,
    args: Vec<OsString>,
    working_directory: Option<PathBuf>,
    read_stdout_lines: bool,
    redirect_stdin: bool,
    job: Option<Arc<ProcessJob>>,
    on_stdout_line: Mutex<Option<LineCallback>>,
    on_stderr_line: Mutex<Option<LineCallback>>,
    on_exited: Mutex<Option<ExitCallback>>,
    start_attempted: bool,
    stdout: Mutex<Option<ChildStdout>>,
    stdin: Mutex<Option<ChildStdin>>,
    running: Option<Arc<Running>>,
}

impl LineProcess {
    pub fn new<P, I, A>(program: P, args: I) -> LineProcess
    where
        P: Into<OsString>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        LineProcess {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            working_directory: None,
            read_stdout_lines: true,
            redirect_stdin: false,
            job: None,
            on_stdout_line: Mutex::new(None),
            on_stderr_line: Mutex::new(None),
            on_exited: Mutex::new(None),
            start_attempted: false,
            stdout: Mutex::new(None),
            stdin: Mutex::new(None),
            running: None,
        }
    }

    pub fn program(&self) -> &OsStr {
        &self.program
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    // ---- configuration (before start) ---------------------------------------------------------

    pub fn working_directory(&mut self, dir: impl Into<PathBuf>) -> &mut Self {
        self.working_directory = Some(dir.into());
        self
    }

    /// true (default): stdout is read line by line into the [`on_stdout_line`](Self::on_stdout_line) callback;
    /// false: stdout is available from [`take_stdout`](Self::take_stdout) after `start()`.
    pub fn read_stdout_lines(&mut self, read: bool) -> &mut Self {
        self.read_stdout_lines = read;
        self
    }

    /// When true, [`take_stdin`](Self::take_stdin) has the child's stdin after `start()`. Otherwise the child
    /// gets no stdin at all (the null device): a GUI has none to pass on, and ffmpeg must never find a console
    /// to read key presses from when the app happens to be started from a terminal.
    pub fn redirect_stdin(&mut self, redirect: bool) -> &mut Self {
        self.redirect_stdin = redirect;
        self
    }

    /// Job object the child is put into right after it starts, so it cannot outlive this process
    /// (see [`ProcessJob`]). Optional: leaving it out only loses that safety net.
    pub fn job(&mut self, job: Option<Arc<ProcessJob>>) -> &mut Self {
        self.job = job;
        self
    }

    /// Called on a reader thread for every line of stdout (without its line ending; "\n", "\r\n" and a lone
    /// "\r" all end a line, so ffmpeg's and mpv's self-overwriting status lines arrive one by one).
    pub fn on_stdout_line(&mut self, callback: impl FnMut(&str) + Send + 'static) -> &mut Self {
        *lock(&self.on_stdout_line) = Some(Box::new(callback));
        self
    }

    /// Called on a reader thread for every line of stderr.
    pub fn on_stderr_line(&mut self, callback: impl FnMut(&str) + Send + 'static) -> &mut Self {
        *lock(&self.on_stderr_line) = Some(Box::new(callback));
        self
    }

    /// Called once with the exit code, after stdout/stderr have been drained: no line arrives after it.
    /// Also called for a process that was stopped or dropped (callers that restart things tell an expected
    /// exit from a surprise by their own bookkeeping, as the C# runners did with a generation counter).
    /// It runs on a background thread and may stop or drop this `LineProcess`.
    pub fn on_exited(&mut self, callback: impl FnOnce(i32) + Send + 'static) -> &mut Self {
        *lock(&self.on_exited) = Some(Box::new(callback));
        self
    }

    // ---- running -----------------------------------------------------------------------------

    pub fn start(&mut self) -> io::Result<()> {
        if self.start_attempted {
            return Err(io::Error::other("already started"));
        }
        self.start_attempted = true;

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(if self.redirect_stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = self.working_directory.as_ref().filter(|d| !d.as_os_str().is_empty()) {
            command.current_dir(dir);
        }
        platform::prepare(&mut command);

        let tree = platform::Tree::new();
        let mut child = command.spawn()?;
        if let Some(job) = &self.job {
            job.assign(&child);
        }
        // After the app's job, never before: a job may only be nested inside one the process is already in.
        tree.adopt(&child);

        let stderr = child.stderr.take().expect("stderr is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        *lock(&self.stdin) = child.stdin.take();

        let mut readers = Vec::with_capacity(2);
        readers.push(spawn_reader("stderr", stderr, lock(&self.on_stderr_line).take()));
        if self.read_stdout_lines {
            readers.push(spawn_reader("stdout", stdout, lock(&self.on_stdout_line).take()));
        } else {
            *lock(&self.stdout) = Some(stdout);
        }

        let running = Arc::new(Running {
            pid: child.id(),
            waiter: platform::Waiter::new(&child),
            child: Mutex::new(child),
            tree,
            exit_code: Mutex::new(None),
            exited: Condvar::new(),
            job: self.job.clone(),
        });
        self.running = Some(running.clone());

        let on_exited = lock(&self.on_exited).take();
        let waiter = thread::Builder::new().name("child-exit".to_string()).spawn(move || {
            let code = running.wait_until_gone();
            // The exit is announced only after the readers hit end of file: no line is delivered after it.
            for reader in readers.into_iter().flatten() {
                let _ = reader.join();
            }
            if let Some(callback) = on_exited {
                callback(code);
            }
        });
        if let Err(e) = waiter {
            // Without the thread nobody would ever notice the exit: do not leave a child nobody watches.
            self.stop();
            return Err(e);
        }
        Ok(())
    }

    /// The child's process id once started.
    pub fn id(&self) -> Option<u32> {
        self.running.as_ref().map(|r| r.pid)
    }

    /// The exit code once the process has ended. A process killed by a signal reports 128 + the signal number.
    pub fn exit_code(&self) -> Option<i32> {
        self.running.as_ref().and_then(|r| *lock(&r.exit_code))
    }

    pub fn has_started(&self) -> bool {
        self.running.is_some()
    }

    pub fn has_exited(&self) -> bool {
        self.running.as_ref().is_some_and(|r| r.has_exited())
    }

    /// The child's raw stdout (when [`read_stdout_lines`](Self::read_stdout_lines) is false); `Some` once.
    pub fn take_stdout(&self) -> Option<ChildStdout> {
        lock(&self.stdout).take()
    }

    /// The child's stdin (when [`redirect_stdin`](Self::redirect_stdin) is true); `Some` once. Dropping it
    /// closes the pipe, which is how the child sees end of input.
    pub fn take_stdin(&self) -> Option<ChildStdin> {
        lock(&self.stdin).take()
    }

    /// The command as a person would type it (see [`arg_quote`]).
    pub fn command_line(&self) -> String {
        arg_quote::command_line(&self.program.to_string_lossy(), self.args.iter().map(|a| a.to_string_lossy()))
    }

    /// Command line with the SRT passphrase and stream-id credentials masked, for log files.
    pub fn safe_command_line(&self) -> String {
        secret_redactor::redact(&self.command_line())
    }

    /// True when the process has ended within `timeout` (or was never started).
    pub fn wait_for_exit(&self, timeout: Duration) -> bool {
        self.running.as_ref().is_none_or(|r| r.wait_for_exit(timeout))
    }

    /// Kills the process tree (no-op when not running). Waits up to 3 s for the exit.
    pub fn stop(&self) {
        if let Some(running) = &self.running {
            running.kill();
            running.wait_for_exit(STOP_WAIT);
        }
    }
}

impl Drop for LineProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for LineProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineProcess")
            .field("command_line", &self.safe_command_line())
            .field("id", &self.id())
            .field("exit_code", &self.exit_code())
            .finish()
    }
}

/// What the owner and the exit thread share about a started child.
struct Running {
    pid: u32,
    child: Mutex<Child>,
    waiter: platform::Waiter,
    tree: platform::Tree,
    exit_code: Mutex<Option<i32>>,
    exited: Condvar,
    job: Option<Arc<ProcessJob>>,
}

impl Running {
    /// Blocks until the process has ended, records the exit code and returns it.
    fn wait_until_gone(&self) -> i32 {
        // Wait without holding the child's lock, so stop() can kill it meanwhile.
        self.waiter.wait();
        // Before the child is reaped: from then on its process id is no longer ours to signal.
        if let Some(job) = &self.job {
            job.release(self.pid);
        }
        let mut child = lock(&self.child);
        let code = child.wait().map(exit_code_of).unwrap_or(-1);
        // Still under the child's lock: kill() must never see "not exited" for a process that was reaped.
        *lock(&self.exit_code) = Some(code);
        self.exited.notify_all();
        code
    }

    fn has_exited(&self) -> bool {
        lock(&self.exit_code).is_some()
    }

    fn wait_for_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut code = lock(&self.exit_code);
        while code.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            code = self.exited.wait_timeout(code, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        true
    }

    fn kill(&self) {
        // Lock order everywhere: the child first, then the exit code.
        let mut child = lock(&self.child);
        // Once the exit thread has reaped the child, its process id (and group) may be somebody else's.
        // Only that thread reaps, and it records the exit code before letting go of the child's lock.
        if lock(&self.exit_code).is_some() {
            return;
        }
        self.tree.kill(&child);
        let _ = child.kill();
    }
}

fn exit_code_of(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A callback that panicked while holding a lock must not take the process handling down with it.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reads `pipe` to its end on a thread of its own, calling `callback` per line. Without a callback the output
/// is still read (and dropped): a child whose pipe is never emptied blocks as soon as the pipe is full.
fn spawn_reader(name: &str, mut pipe: impl Read + Send + 'static, mut callback: Option<LineCallback>) -> Option<JoinHandle<()>> {
    let read = move || {
        let mut lines = LineSplitter::default();
        let mut deliver = |line: &str| {
            if let Some(cb) = callback.as_mut() {
                // A panicking callback loses its lines, not the pipe: the child must not block on it.
                if catch_unwind(AssertUnwindSafe(|| cb(line))).is_err() {
                    callback = None;
                }
            }
        };
        let mut buffer = [0u8; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => lines.feed(&buffer[..n], &mut deliver),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break, // the pipe broke: the child is gone
            }
        }
        lines.finish(&mut deliver);
    };
    thread::Builder::new().name(format!("child-{name}")).spawn(read).ok()
}

/// A line that never ends is cut here, so a child that pours binary data into a text pipe cannot eat the memory.
const MAX_LINE_BYTES: usize = 1 << 20;

/// Cuts a byte stream into lines the way .NET's process output reader did: "\n", "\r\n" and a lone "\r" each end
/// a line (also when "\r\n" is split across two reads), empty lines are delivered, and a last line without an
/// ending is delivered at end of file. Text is UTF-8; invalid bytes become U+FFFD; a leading byte order mark is
/// dropped.
#[derive(Default)]
struct LineSplitter {
    pending: Vec<u8>,
    after_cr: bool,
    past_start: bool,
}

impl LineSplitter {
    fn feed(&mut self, bytes: &[u8], deliver: &mut impl FnMut(&str)) {
        for &b in bytes {
            let after_cr = std::mem::replace(&mut self.after_cr, false);
            match b {
                b'\n' if after_cr => {} // the second half of "\r\n"
                b'\n' => self.emit(deliver),
                b'\r' => {
                    self.emit(deliver);
                    self.after_cr = true;
                }
                _ => {
                    self.pending.push(b);
                    if self.pending.len() >= MAX_LINE_BYTES {
                        self.emit(deliver);
                    }
                }
            }
        }
    }

    fn finish(&mut self, deliver: &mut impl FnMut(&str)) {
        if !self.pending.is_empty() {
            self.emit(deliver);
        }
    }

    fn emit(&mut self, deliver: &mut impl FnMut(&str)) {
        let mut line = self.pending.as_slice();
        if !std::mem::replace(&mut self.past_start, true) {
            line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
        }
        deliver(&String::from_utf8_lossy(line));
        self.pending.clear();
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};
    use windows::Win32::System::Threading::{WaitForSingleObject, CREATE_NO_WINDOW, INFINITE};

    pub fn prepare(command: &mut Command) {
        // ffmpeg and mpv are console programs: without this each start flashes a console window.
        command.creation_flags(CREATE_NO_WINDOW.0);
    }

    /// Waits for the process through its handle, leaving the `Child` free for kill() meanwhile.
    pub struct Waiter(HANDLE);

    // SAFETY: a process handle may be waited on from any thread.
    unsafe impl Send for Waiter {}
    unsafe impl Sync for Waiter {}

    impl Waiter {
        /// The handle stays owned by `child`; `Running` keeps both for the same lifetime.
        pub fn new(child: &Child) -> Waiter {
            Waiter(HANDLE(child.as_raw_handle()))
        }

        pub fn wait(&self) {
            // SAFETY: the handle is the child's process handle, alive as long as the `Child` next to us.
            unsafe { WaitForSingleObject(self.0, INFINITE) };
        }
    }

    /// The child and everything it starts, as a job object of its own, so that stop() takes the whole tree
    /// in one call (the C# apps ran "taskkill /T /F" for this). Nested inside the app's kill-on-close job.
    pub struct Tree(Option<HANDLE>);

    // SAFETY: a job object handle is a kernel handle; the calls made on it here are thread-safe.
    unsafe impl Send for Tree {}
    unsafe impl Sync for Tree {}

    impl Tree {
        pub fn new() -> Tree {
            // SAFETY: plain Win32 call; failure (no job available in this sandbox) leaves a plain kill().
            Tree(unsafe { CreateJobObjectW(None, PCWSTR::null()) }.ok())
        }

        pub fn adopt(&self, child: &Child) {
            if let Some(job) = self.0 {
                // SAFETY: both handles are valid for the duration of the call.
                let _ = unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) };
            }
        }

        pub fn kill(&self, _child: &Child) {
            if let Some(job) = self.0 {
                // SAFETY: the job handle is ours and still open.
                let _ = unsafe { TerminateJobObject(job, 1) };
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            if let Some(job) = self.0.take() {
                // SAFETY: created in `new`, closed exactly once. No kill-on-close limit is set on this job, so
                // closing it leaves a still-running child alone.
                let _ = unsafe { CloseHandle(job) };
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub fn prepare(command: &mut Command) {
        // A process group of its own, so that stop() can signal the child together with whatever it started.
        command.process_group(0);
    }

    /// Waits for the process without reaping it, leaving the `Child` free for kill() meanwhile.
    pub struct Waiter(libc::pid_t);

    impl Waiter {
        pub fn new(child: &Child) -> Waiter {
            Waiter(child.id() as libc::pid_t)
        }

        pub fn wait(&self) {
            loop {
                // SAFETY: `info` is a valid siginfo_t to write into. WNOWAIT leaves the zombie in place, so the
                // process id stays ours until `Child::wait` reaps it.
                let result = unsafe {
                    let mut info: libc::siginfo_t = std::mem::zeroed();
                    libc::waitid(libc::P_PID, self.0 as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT)
                };
                if result == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
            }
        }
    }

    /// The child's process group.
    pub struct Tree;

    impl Tree {
        pub fn new() -> Tree {
            Tree
        }

        pub fn adopt(&self, _child: &Child) {}

        pub fn kill(&self, child: &Child) {
            // SAFETY: `kill` has no memory-safety preconditions. The caller holds the child's lock and has just
            // seen it not reaped, so the id (and the group it leads) is still the child's.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut splitter = LineSplitter::default();
        let mut deliver = |line: &str| lines.push(line.to_string());
        for chunk in chunks {
            splitter.feed(chunk, &mut deliver);
        }
        splitter.finish(&mut deliver);
        lines
    }

    #[test]
    fn every_kind_of_line_ending_ends_a_line() {
        assert_eq!(split(&[b"a\nb\r\nc\rd"]), ["a", "b", "c", "d"]);
        assert_eq!(split(&[b"a\n\nb\n"]), ["a", "", "b"]);
        assert_eq!(split(&[b"\r\n\r\n"]), ["", ""]);
        assert_eq!(split(&[b"\n\r"]), ["", ""]);
        assert_eq!(split(&[b""]), Vec::<String>::new());
        assert_eq!(split(&[b"no ending"]), ["no ending"]);
    }

    #[test]
    fn a_status_line_rewritten_with_carriage_returns_arrives_update_by_update() {
        assert_eq!(split(&[b"frame=1\rframe=2\rframe=3\n"]), ["frame=1", "frame=2", "frame=3"]);
    }

    #[test]
    fn a_line_ending_split_across_two_reads_is_one_ending() {
        assert_eq!(split(&[b"a\r", b"\nb"]), ["a", "b"]);
        assert_eq!(split(&[b"a\r", b"b"]), ["a", "b"]);
        assert_eq!(split(&[b"a\r", b"\n", b"\nb"]), ["a", "", "b"]);
        assert_eq!(split(&[b"a", b"b", b"\n"]), ["ab"]);
    }

    #[test]
    fn text_is_utf8_whatever_the_code_page_and_survives_being_cut_mid_character() {
        let text = "直播 ライブ\n".as_bytes();
        assert_eq!(split(&[&text[..4], &text[4..]]), ["直播 ライブ"]);
        assert_eq!(split(&[b"\xEF\xBB\xBFfirst\n\xEF\xBB\xBFsecond"]), ["first", "\u{feff}second"]);
        assert_eq!(split(&[b"bad \xFF byte\n"]), ["bad \u{fffd} byte"]);
    }

    #[test]
    fn an_endless_line_is_cut_instead_of_growing_forever() {
        let chunk = vec![b'x'; MAX_LINE_BYTES / 2 + 1];
        let lines = split(&[&chunk, &chunk, &chunk]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
    }
}
