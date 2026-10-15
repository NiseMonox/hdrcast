//! The engine's pipes on unix: named FIFOs under the temp directory, read by ffmpeg.
//!
//! How this maps onto [`PipeServer`]:
//! * The FIFO node is made when the server is created, so [`pipe_listening`] (a `stat`) sees it from then
//!   on. Unlike on Windows, looking at it consumes nothing.
//! * A FIFO cannot be opened for writing "with a timeout": the blocking open waits for a reader forever.
//!   So [`FifoServer::wait_for_client`] opens with `O_NONBLOCK`, which fails with `ENXIO` until a reader has
//!   opened the other end, and retries every few milliseconds.
//! * The descriptor then STAYS non-blocking and [`FifoServer::write_all`] blocks in `poll` instead of in
//!   `write`. That is what makes the [`PipeCanceller`] sound: a thread blocked in a plain `write` on a full
//!   pipe can only be released by the reader, or by closing the descriptor from another thread - which
//!   macOS does not promise to notice and which races with the descriptor number being reused. `poll`
//!   watches a second descriptor (one end of a socket pair the canceller writes a byte into) at the same
//!   time, so cancelling is an ordinary wake-up and no descriptor is ever closed while in use.
//! * A reader that goes away turns the next `write` into `EPIPE` (SIGPIPE is ignored process-wide).
//! * One owner per name, like `FILE_FLAG_FIRST_PIPE_INSTANCE`: an advisory lock on `<fifo>.lock`, held for
//!   the life of the server and released by the kernel even if the engine is killed. Holding it, a FIFO
//!   left behind by a killed engine is simply taken over (a FIFO keeps no data, and a reader that already
//!   opened the old node gets connected), and the node can be removed on drop without taking it away from
//!   a successor.
//!
//! The kernel buffer of a FIFO is small and fixed on macOS (16 - 64 KiB; there is no `F_SETPIPE_SZ`), so
//! `out_buffer_bytes` cannot be honoured: a frame goes through in many short writes, each of which waits for
//! ffmpeg to read. The slack of design.md section 2.2 comes from the frame queue in front of the pipe.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::signals::ignore_sigpipe;
use crate::platform::{PipeCanceller, PipeServer};

/// How often a pending [`FifoServer::wait_for_client`] looks for a reader.
const CONNECT_POLL: Duration = Duration::from_millis(5);
/// Upper bound of one wait for room in the pipe; the write is retried afterwards whatever `poll` said, so
/// a missed wake-up costs this much and not the session.
const WRITE_POLL: Duration = Duration::from_millis(100);
/// How long a new server waits for the previous owner of its name to finish going away.
const OWNER_WAIT: Duration = Duration::from_secs(2);

/// The path ffmpeg opens for the pipe `name`: `<temp dir>/<name>.fifo`.
pub(crate) fn pipe_path(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("{name}.fifo"))
        .to_string_lossy()
        .into_owned()
}

/// True when the FIFO of pipe `name` exists. Only a `stat`: nothing is opened.
pub(crate) fn pipe_listening(name: &str) -> bool {
    std::fs::symlink_metadata(pipe_path(name)).is_ok_and(|meta| meta.file_type().is_fifo())
}

fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "the pipe operation was cancelled",
    )
}

/// Set once by the canceller; from then on every wait and write of the server fails.
struct CancelSignal {
    set: AtomicBool,
    /// The canceller writes one byte here ...
    notify: UnixStream,
    /// ... which makes this end readable for `poll`. Never drained: the signal is permanent.
    watch: UnixStream,
}

impl CancelSignal {
    fn new() -> io::Result<Self> {
        let (notify, watch) = UnixStream::pair()?;
        notify.set_nonblocking(true)?;
        Ok(Self {
            set: AtomicBool::new(false),
            notify,
            watch,
        })
    }

    fn cancel(&self) {
        if !self.set.swap(true, Ordering::SeqCst) {
            // Non-blocking, into an empty socket buffer: this cannot fail for lack of room.
            let _ = (&self.notify).write(&[1]);
        }
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
}

/// `poll` on `fds`. An interrupting signal counts as "nothing happened yet".
fn poll(fds: &mut [libc::pollfd], timeout: Duration) -> io::Result<()> {
    // Round up: a wait of half a millisecond must not turn into a busy loop.
    let millis = timeout.as_nanos().div_ceil(1_000_000).min(60_000) as libc::c_int;
    // SAFETY: `fds` points to `fds.len()` initialised pollfd records that stay valid for the call; every
    // descriptor in it is owned by the caller's object and therefore open.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

/// The advisory lock that makes one engine the owner of a pipe name: an exclusive `flock` on the file
/// `<fifo>.lock`, which the kernel releases when the process ends, however it ends.
struct OwnerLock {
    /// Holds the lock for as long as it is open.
    _file: File,
    lock_path: String,
}

impl OwnerLock {
    fn acquire(fifo_path: &str) -> io::Result<Self> {
        let lock_path = format!("{fifo_path}.lock");
        let deadline = Instant::now() + OWNER_WAIT;
        loop {
            // Opened anew on every attempt: an owner that goes away removes the file, and a lock on a
            // file that is no longer at the path would be a lock nobody else can see.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path)?;
            // SAFETY: `file` is an open descriptor owned by this function; flock only changes its lock state.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                if is_file_at(&file, &lock_path) {
                    return Ok(Self {
                        _file: file,
                        lock_path,
                    });
                }
                // Locked the file of an owner that removed it in between: try again on the current one.
            } else {
                let error = io::Error::last_os_error();
                match error.kind() {
                    // held by another engine (or the call was interrupted): wait and try again
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {}
                    _ => return Err(error),
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another capture engine is serving a pipe of this name",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for OwnerLock {
    fn drop(&mut self) {
        // Removed while still locked (the file closes after this), so whoever locks next either finds no
        // file and makes a new one, or notices in `acquire` that the one it locked is gone.
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// True when `file` is the file that `lock_path` names right now.
fn is_file_at(file: &File, lock_path: &str) -> bool {
    match (file.metadata(), std::fs::symlink_metadata(lock_path)) {
        (Ok(open), Ok(named)) => open.dev() == named.dev() && open.ino() == named.ino(),
        _ => false,
    }
}

/// Makes the FIFO node, or takes over one that is already there. Call with the owner lock held.
fn make_fifo(path: &str) -> io::Result<()> {
    let c_path = std::ffi::CString::new(path).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the pipe path contains a NUL byte",
        )
    })?;
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call.
    if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.kind() != io::ErrorKind::AlreadyExists {
        return Err(error);
    }
    // Something has this name already. symlink_metadata does not follow a link, so what is checked here is
    // the node itself.
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_fifo() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a file that is not a FIFO is in the way",
        ));
    }
    // SAFETY: plain call without arguments; it cannot fail.
    if meta.uid() != unsafe { libc::getuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the FIFO belongs to another user",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// The writing end of one FIFO.
pub(crate) struct FifoServer {
    path: String,
    /// The write end once a reader is there. Non-blocking, see the module documentation.
    pipe: Option<File>,
    cancel: Arc<CancelSignal>,
    /// Declared after `pipe`: the lock goes last, after the node was removed in `drop`.
    _owner: OwnerLock,
}

impl FifoServer {
    /// Creates the FIFO of pipe `name` (mode 0600) and becomes its only owner.
    pub(crate) fn create(name: &str) -> io::Result<Self> {
        ignore_sigpipe();
        let path = pipe_path(name);
        let cancel = Arc::new(CancelSignal::new()?);
        let owner = OwnerLock::acquire(&path)?;
        make_fifo(&path)?;
        Ok(Self {
            path,
            pipe: None,
            cancel,
            _owner: owner,
        })
    }

    /// One attempt to open the write end. `Ok(None)`: no reader has opened the FIFO yet.
    fn try_connect(&self) -> io::Result<Option<File>> {
        let opened = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path);
        match opened {
            Ok(pipe) => {
                // The name could have been pointed at something else since the node was made.
                if !pipe.metadata()?.file_type().is_fifo() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the pipe path no longer names a FIFO",
                    ));
                }
                Ok(Some(pipe))
            }
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Sleeps for `duration` or until the canceller fires, whichever comes first.
    fn pause(&self, duration: Duration) -> io::Result<()> {
        let mut fds = [libc::pollfd {
            fd: self.cancel.watch.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        poll(&mut fds, duration)?;
        if self.cancel.is_set() {
            return Err(cancelled());
        }
        Ok(())
    }

    /// Blocks until the pipe may take more data, the reader is gone, the canceller fired or [`WRITE_POLL`]
    /// passed. Only cancellation is reported: for everything else the next `write` has the answer.
    fn wait_for_room(&self, pipe: &File) -> io::Result<()> {
        let mut fds = [
            libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            },
            libc::pollfd {
                fd: self.cancel.watch.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        poll(&mut fds, WRITE_POLL)?;
        if self.cancel.is_set() {
            return Err(cancelled());
        }
        Ok(())
    }
}

impl PipeServer for FifoServer {
    fn wait_for_client(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.pipe.is_some() {
            return Ok(true);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancel.is_set() {
                return Err(cancelled());
            }
            if let Some(pipe) = self.try_connect()? {
                self.pipe = Some(pipe);
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            self.pause(left.min(CONNECT_POLL))?;
        }
    }

    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        let Some(pipe) = self.pipe.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no reader has opened the pipe",
            ));
        };
        while !data.is_empty() {
            if self.cancel.is_set() {
                return Err(cancelled());
            }
            let mut writer: &File = pipe;
            match writer.write(data) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => data = &data[written..],
                // the pipe is full: wait for the reader (or the canceller)
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_for_room(pipe)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // EPIPE: the reader went away
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn canceller(&self) -> PipeCanceller {
        let cancel = Arc::clone(&self.cancel);
        PipeCanceller::new(move || cancel.cancel())
    }
}

impl Drop for FifoServer {
    fn drop(&mut self) {
        // Closing the write end is the reader's end of file; data it has not read yet stays readable.
        self.pipe = None;
        // Still the owner (the lock is released after this), so this cannot remove a successor's node.
        let _ = std::fs::remove_file(&self.path);
    }
}

// These tests need a unix system; on the Windows machine this backend was written on they are compiled for
// the macOS targets (`cargo check --all-targets`) but have never been run.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;

    /// A pipe name no other test (or test process) uses.
    fn unique(tag: &str) -> String {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        format!(
            "hdrcast-test-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// What a `stat` of the FIFO node of `name` says. Looking at a FIFO is not a connection.
    fn node(name: &str) -> std::fs::Metadata {
        let fifo = pipe_path(name);
        std::fs::symlink_metadata(fifo).unwrap()
    }

    fn connect(server: &mut FifoServer) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.wait_for_client(Duration::from_millis(50)).unwrap() {
            assert!(Instant::now() < deadline, "the reader never connected");
        }
    }

    #[test]
    fn the_fifo_exists_while_the_server_does_and_looking_does_not_connect() {
        let name = unique("exists");
        assert!(!pipe_listening(&name));
        let mut server = FifoServer::create(&name).unwrap();
        let meta = node(&name);
        assert!(meta.file_type().is_fifo());
        assert_eq!(meta.mode() & 0o777, 0o600);
        // Asking any number of times is not a client.
        for _ in 0..3 {
            assert!(pipe_listening(&name));
        }
        assert!(!server.wait_for_client(Duration::from_millis(30)).unwrap());
        assert!(!server.wait_for_client(Duration::ZERO).unwrap());
        let lock = format!("{}.lock", pipe_path(&name));
        assert!(std::fs::symlink_metadata(&lock).is_ok(), "the FIFO's lock");
        drop(server);
        assert!(!pipe_listening(&name));
        assert!(
            std::fs::symlink_metadata(&lock).is_err(),
            "the lock goes with its FIFO"
        );
    }

    #[test]
    fn bytes_arrive_in_order_and_closing_is_the_readers_end_of_file() {
        let name = unique("data");
        let mut server = FifoServer::create(&name).unwrap();
        let path = pipe_path(&name);
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            File::open(path).unwrap().read_to_end(&mut all).unwrap();
            all
        });
        connect(&mut server);
        assert!(server.wait_for_client(Duration::ZERO).unwrap());
        // Far more than the kernel buffer of a FIFO, so the writer has to wait for the reader many times.
        let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        server.write_all(&payload).unwrap();
        server.write_all(b"tail").unwrap();
        drop(server);
        let received = reader.join().unwrap();
        assert_eq!(received.len(), payload.len() + 4);
        assert!(received[..payload.len()] == payload[..]);
        assert_eq!(&received[payload.len()..], b"tail");
    }

    #[test]
    fn a_reader_that_goes_away_breaks_the_pipe() {
        let name = unique("broken");
        let mut server = FifoServer::create(&name).unwrap();
        // Opened without blocking, so the reader can exist before the writer on one thread.
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(pipe_path(&name))
            .unwrap();
        connect(&mut server);
        server.write_all(b"hello").unwrap();
        drop(reader);
        let error = server.write_all(&[0u8; 1 << 20]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{error}");
    }

    #[test]
    fn the_canceller_releases_a_writer_nobody_reads_from() {
        let name = unique("cancel");
        let mut server = FifoServer::create(&name).unwrap();
        // A reader that opens the FIFO and then never reads.
        let reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(pipe_path(&name))
            .unwrap();
        connect(&mut server);
        let canceller = server.canceller();
        let (done, result) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            let outcome = server.write_all(&vec![7u8; 8 << 20]);
            let _ = done.send(outcome.map_err(|e| e.kind()));
            server
        });
        // 8 MiB cannot fit: the writer is stuck.
        assert!(result.recv_timeout(Duration::from_millis(300)).is_err());
        canceller.cancel();
        let outcome = result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, Err(io::ErrorKind::Interrupted));
        // ... and stays cancelled.
        let mut server = writer.join().unwrap();
        assert!(server.write_all(b"x").is_err());
        drop(reader);
    }

    #[test]
    fn cancelling_ends_the_wait_for_a_client() {
        let name = unique("cancel-wait");
        let mut server = FifoServer::create(&name).unwrap();
        let canceller = server.canceller();
        let started = Instant::now();
        let waiter = std::thread::spawn(move || server.wait_for_client(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(100));
        canceller.cancel();
        let error = waiter.join().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_name_has_one_owner_and_a_leftover_fifo_is_taken_over() {
        let name = unique("owner");
        let first = FifoServer::create(&name).unwrap();
        // While the first lives a second server of the same name is refused (after OWNER_WAIT).
        let error = FifoServer::create(&name).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse, "{error}");
        assert!(
            pipe_listening(&name),
            "the refused server must not remove the node"
        );
        drop(first);

        // What a killed engine leaves behind: the node without an owner.
        let path = std::ffi::CString::new(pipe_path(&name)).unwrap();
        // SAFETY: NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
        let second = FifoServer::create(&name).unwrap();
        let meta = node(&name);
        assert_eq!(meta.mode() & 0o777, 0o600);
        drop(second);
        assert!(!pipe_listening(&name));
        let _ = std::fs::remove_file(format!("{}.lock", pipe_path(&name)));
    }

    #[test]
    fn a_regular_file_in_the_way_is_not_touched() {
        let name = unique("file");
        std::fs::write(pipe_path(&name), b"keep me").unwrap();
        assert!(!pipe_listening(&name));
        let error = FifoServer::create(&name).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(std::fs::read(pipe_path(&name)).unwrap(), b"keep me");
        let _ = std::fs::remove_file(pipe_path(&name));
        let _ = std::fs::remove_file(format!("{}.lock", pipe_path(&name)));
    }
}
