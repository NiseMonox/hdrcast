//! A two-way byte stream to mpv's JSON IPC endpoint: a named pipe on Windows, a Unix socket elsewhere.
//!
//! One thread reads while others write, so on Windows the pipe is opened for overlapped I/O: on an ordinary
//! (synchronous) handle a blocked read holds the handle's lock and every write would wait behind it.

use std::io;

#[cfg(windows)]
pub use win::IpcStream;

#[cfg(not(windows))]
pub use unix::IpcStream;

#[cfg(windows)]
mod win {
    use super::io;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    use windows::Win32::System::Threading::CreateEventW;
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    struct Owned(HANDLE);
    // SAFETY: a kernel handle may be used from any thread; overlapped I/O is what makes concurrent use sound.
    unsafe impl Send for Owned {}
    unsafe impl Sync for Owned {}

    impl Drop for Owned {
        fn drop(&mut self) {
            // SAFETY: the handle is ours and nothing uses it after the last clone is gone.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// Cheap to clone: every clone is the same connection.
    #[derive(Clone)]
    pub struct IpcStream {
        pipe: Arc<Owned>,
    }

    fn to_io(e: windows::core::Error) -> io::Error {
        io::Error::from_raw_os_error(e.code().0 & 0xffff)
    }

    impl IpcStream {
        /// Fails with `NotFound` until mpv has created the pipe.
        pub fn connect(endpoint: &str) -> io::Result<Self> {
            let wide: Vec<u16> = std::ffi::OsStr::new(endpoint).encode_wide().chain(Some(0)).collect();
            // SAFETY: `wide` is a NUL-terminated path that outlives the call.
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .map_err(to_io)?;
            Ok(Self { pipe: Arc::new(Owned(handle)) })
        }

        /// Makes a read or write blocked in another thread return with an error.
        pub fn shutdown(&self) {
            // SAFETY: cancelling I/O on our own open handle.
            unsafe {
                let _ = CancelIoEx(self.pipe.0, None);
            }
        }

        /// Runs one overlapped operation to completion and returns the bytes transferred.
        fn complete(&self, start: impl FnOnce(&mut OVERLAPPED) -> windows::core::Result<()>) -> io::Result<usize> {
            // SAFETY: a manual-reset, initially unset, unnamed event; closed below.
            let event = unsafe { CreateEventW(None, true, false, None) }.map_err(to_io)?;
            let mut overlapped = OVERLAPPED { hEvent: event, ..Default::default() };
            let started = start(&mut overlapped);
            let result = match started {
                Ok(()) => Ok(()),
                Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => Ok(()),
                Err(e) => Err(to_io(e)),
            };
            let result = result.and_then(|()| {
                let mut n = 0u32;
                // SAFETY: `overlapped` is the structure the operation was started with and stays in place until
                // this returns (it waits for completion).
                unsafe { GetOverlappedResult(self.pipe.0, &overlapped, &mut n, true) }.map_err(to_io)?;
                Ok(n as usize)
            });
            // SAFETY: the event is ours and the operation using it has finished or failed to start.
            unsafe {
                let _ = CloseHandle(event);
            }
            result
        }
    }

    impl io::Read for IpcStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let handle = self.pipe.0;
            match self.complete(|ov| {
                // SAFETY: `buf` and `ov` stay valid until `complete` has waited for the operation.
                unsafe { ReadFile(handle, Some(buf), None, Some(ov)) }
            }) {
                Ok(n) => Ok(n),
                // the other end closed the pipe: end of stream, not an error
                Err(e) if e.raw_os_error() == Some(109) || e.raw_os_error() == Some(233) => Ok(0),
                Err(e) => Err(e),
            }
        }
    }

    impl io::Write for IpcStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let handle = self.pipe.0;
            self.complete(|ov| {
                // SAFETY: as for the read.
                unsafe { WriteFile(handle, Some(buf), None, Some(ov)) }
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod unix {
    use super::io;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    #[derive(Clone)]
    pub struct IpcStream {
        socket: Arc<UnixStream>,
    }

    impl IpcStream {
        /// Fails with `NotFound` / `ConnectionRefused` until mpv listens on the socket.
        pub fn connect(endpoint: &str) -> io::Result<Self> {
            Ok(Self { socket: Arc::new(UnixStream::connect(endpoint)?) })
        }

        pub fn shutdown(&self) {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
    }

    impl io::Read for IpcStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self.socket).read(buf)
        }
    }

    impl io::Write for IpcStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self.socket).write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
