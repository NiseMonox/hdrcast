//! Named-pipe server (design.md sections 2.2, 2.7): one outbound byte pipe per stream, read by ffmpeg.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use super::util::{io_error, to_wide, Handle};
use crate::platform::{PipeCanceller, PipeServer};

pub(crate) fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// True when an instance of the pipe waits for a client. Only asks: it never connects, unlike any open.
pub(crate) fn pipe_listening(name: &str) -> bool {
    let path = to_wide(&pipe_path(name));
    // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call.
    unsafe { WaitNamedPipeW(PCWSTR(path.as_ptr()), 1).as_bool() }
}

/// One instance of an outbound, byte-mode named pipe.
///
/// The handle is opened for overlapped I/O so that waiting for the client can time out and a write nobody
/// reads can be aborted from another thread ([`PipeCanceller`]); every operation still blocks its caller
/// until it completed or was cancelled, so the caller's buffers never outlive a pending operation.
pub(crate) struct NamedPipeServer {
    pipe: Handle,
    /// Manual-reset event the kernel signals when the overlapped operation in flight completes.
    io_event: Handle,
    /// Manual-reset event set by the canceller; once set, every wait and write fails.
    cancel_event: Arc<Handle>,
    /// Boxed: the kernel keeps this address while a connect is pending across `wait_for_client` calls.
    overlapped: Box<OVERLAPPED>,
    connect_pending: bool,
    connected: bool,
}

// SAFETY: the handles are process-wide kernel object references usable from any thread; the OVERLAPPED block
// (which holds a raw event handle and is therefore not automatically Send) is only touched through
// `&mut self`, i.e. by the one thread that owns the server at that moment.
unsafe impl Send for NamedPipeServer {}

impl NamedPipeServer {
    pub(crate) fn create(name: &str, out_buffer_bytes: usize) -> io::Result<Self> {
        let path = to_wide(&pipe_path(name));
        let out_buffer = u32::try_from(out_buffer_bytes).unwrap_or(u32::MAX);
        // SAFETY: `path` is a NUL-terminated UTF-16 string that outlives the call. The flags describe a
        // single-instance (the name cannot be squatted), outbound, byte-mode pipe for local clients.
        let raw = unsafe {
            CreateNamedPipeW(
                PCWSTR(path.as_ptr()),
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                out_buffer,
                0,
                0,
                None,
            )
        };
        if raw.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is the valid handle just created; from here it is owned (and closed) by `pipe`.
        let pipe = unsafe { Handle::from_raw(raw) };
        let io_event = new_manual_reset_event()?;
        let cancel_event = Arc::new(new_manual_reset_event()?);
        Ok(Self {
            pipe,
            io_event,
            cancel_event,
            overlapped: Box::new(OVERLAPPED::default()),
            connect_pending: false,
            connected: false,
        })
    }

    /// Prepares the OVERLAPPED block for a new operation.
    fn reset_overlapped(&mut self) {
        *self.overlapped = OVERLAPPED {
            hEvent: self.io_event.raw(),
            ..Default::default()
        };
    }

    /// Waits for the operation in flight or the canceller.
    fn wait_io(&self, timeout_ms: u32) -> WAIT_EVENT {
        let handles: [HANDLE; 2] = [self.io_event.raw(), self.cancel_event.raw()];
        // SAFETY: both handles are live events owned by `self`.
        unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) }
    }

    /// Cancels the operation in flight and waits until the kernel let go of the OVERLAPPED block and of the
    /// caller's buffer.
    fn abort_io(&mut self) {
        let mut transferred = 0u32;
        // SAFETY: `pipe` is live and `overlapped` is the block of the pending operation; waiting for the
        // (cancelled) result guarantees the kernel no longer references it when this returns.
        unsafe {
            let _ = CancelIoEx(self.pipe.raw(), Some(&*self.overlapped));
            let _ = GetOverlappedResult(self.pipe.raw(), &*self.overlapped, &mut transferred, true);
        }
    }

    /// Fetches the result of the completed operation.
    fn finish_io(&mut self) -> io::Result<usize> {
        let mut transferred = 0u32;
        // SAFETY: `pipe` is live and `overlapped` belongs to an operation that already completed.
        unsafe { GetOverlappedResult(self.pipe.raw(), &*self.overlapped, &mut transferred, false) }
            .map_err(io_error)?;
        Ok(transferred as usize)
    }

    /// One WriteFile: blocks until `data` was taken by the pipe (the reader consumed enough of it).
    fn write_some(&mut self, data: &[u8]) -> io::Result<usize> {
        // SAFETY: the cancel event is a live handle owned by `self`.
        if unsafe { WaitForSingleObject(self.cancel_event.raw(), 0) } == WAIT_OBJECT_0 {
            return Err(cancelled());
        }
        self.reset_overlapped();
        // SAFETY: `pipe` is live; `data` and the OVERLAPPED block stay valid until the operation completed
        // or was cancelled, both of which are awaited below before returning.
        let started = unsafe {
            WriteFile(
                self.pipe.raw(),
                Some(data),
                None,
                Some(&mut *self.overlapped),
            )
        };
        match started {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                let event = self.wait_io(INFINITE);
                if event != WAIT_OBJECT_0 {
                    self.abort_io();
                    return Err(if event.0 == WAIT_OBJECT_0.0 + 1 {
                        cancelled()
                    } else {
                        io::Error::last_os_error()
                    });
                }
            }
            Err(e) => return Err(io_error(e)),
        }
        self.finish_io()
    }
}

fn new_manual_reset_event() -> io::Result<Handle> {
    // SAFETY: no attributes, no name; on success the returned handle is ours to close.
    unsafe {
        CreateEventW(None, true, false, PCWSTR::null())
            .map(|h| Handle::from_raw(h))
            .map_err(io_error)
    }
}

fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "the pipe operation was cancelled",
    )
}

impl PipeServer for NamedPipeServer {
    fn wait_for_client(&mut self, timeout: Duration) -> io::Result<bool> {
        if self.connected {
            return Ok(true);
        }
        if !self.connect_pending {
            self.reset_overlapped();
            // SAFETY: `pipe` is live and the boxed OVERLAPPED block lives as long as `self`; a connect that
            // is still pending when `self` goes away is cancelled and awaited in Drop.
            match unsafe { ConnectNamedPipe(self.pipe.raw(), Some(&mut *self.overlapped)) } {
                Ok(()) => {
                    self.connected = true;
                    return Ok(true);
                }
                // The client was faster than this call.
                Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                    self.connected = true;
                    return Ok(true);
                }
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => self.connect_pending = true,
                Err(e) => return Err(io_error(e)),
            }
        }
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1);
        let event = self.wait_io(timeout_ms);
        if event == WAIT_OBJECT_0 {
            self.connect_pending = false;
            self.finish_io()?;
            self.connected = true;
            Ok(true)
        } else if event == WAIT_TIMEOUT {
            Ok(false)
        } else if event.0 == WAIT_OBJECT_0.0 + 1 {
            self.abort_io();
            self.connect_pending = false;
            Err(cancelled())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        // A whole frame goes out in one WriteFile; the loop only matters beyond 1 GiB (the length is a u32).
        const MAX_WRITE: usize = 1 << 30;
        while !data.is_empty() {
            let chunk = &data[..data.len().min(MAX_WRITE)];
            let written = self.write_some(chunk)?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            data = &data[written..];
        }
        Ok(())
    }

    fn canceller(&self) -> PipeCanceller {
        let cancel_event = Arc::clone(&self.cancel_event);
        PipeCanceller::new(move || {
            // SAFETY: the event handle is kept alive by the Arc captured here.
            let _ = unsafe { SetEvent(cancel_event.raw()) };
        })
    }
}

impl Drop for NamedPipeServer {
    fn drop(&mut self) {
        if self.connect_pending {
            self.abort_io();
        }
        // The handles close after this; data the client has not read yet stays readable on its end.
    }
}
