//! JSON IPC client for a running mpv (`--input-ipc-server=<endpoint>`). Connects in the background (retrying
//! until mpv has created the endpoint or `keep_trying` says stop), observes the status properties, hands every
//! received message to a callback and offers thread-safe command sending.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use hdrcast_media::receiver::mpv_ipc::{self, MpvIpcMessage};
use serde_json::Value;

use crate::ipc_stream::IpcStream;

type LogFn = Box<dyn Fn(&str) + Send + Sync>;
type MessageFn = Box<dyn Fn(&MpvIpcMessage) + Send + Sync>;
type ConnectedFn = Box<dyn Fn() + Send + Sync>;

pub struct Handlers {
    pub log: LogFn,
    pub message: MessageFn,
    pub connected: ConnectedFn,
}

struct Shared {
    endpoint: String,
    handlers: Handlers,
    writer: Mutex<Option<IpcStream>>,
    connected: AtomicBool,
    disposed: AtomicBool,
    request_id: AtomicI64,
}

#[derive(Clone)]
pub struct MpvIpcClient {
    shared: Arc<Shared>,
}

impl MpvIpcClient {
    /// Starts the connect/read loop. `keep_trying` is polled between connect attempts.
    pub fn start(endpoint: &str, handlers: Handlers, keep_trying: impl Fn() -> bool + Send + 'static) -> Self {
        let shared = Arc::new(Shared {
            endpoint: endpoint.to_string(),
            handlers,
            writer: Mutex::new(None),
            connected: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            request_id: AtomicI64::new(0),
        });
        let client = Self { shared };
        let me = client.clone();
        thread::Builder::new().name("mpv-ipc".into()).spawn(move || me.run(keep_trying)).ok();
        client
    }

    fn log(&self, line: &str) {
        (self.shared.handlers.log)(line);
    }

    fn run(&self, keep_trying: impl Fn() -> bool) {
        let s = &self.shared;
        let mut complained = false;
        let stream = loop {
            if s.disposed.load(Ordering::SeqCst) {
                return;
            }
            if !keep_trying() {
                self.log("IPC: giving up (mpv is not running)");
                return;
            }
            match IpcStream::connect(&s.endpoint) {
                Ok(stream) => break stream,
                Err(e) => {
                    // mpv has not created the endpoint yet (or all its pipe instances are busy): expected
                    let waiting = matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused)
                        || e.raw_os_error() == Some(231);
                    if !waiting && !complained {
                        complained = true;
                        self.log(&format!("IPC: connect failed: {e}"));
                    }
                    thread::sleep(Duration::from_millis(if waiting { 200 } else { 500 }));
                }
            }
        };
        if s.disposed.load(Ordering::SeqCst) {
            stream.shutdown();
            return;
        }

        *s.writer.lock().unwrap() = Some(stream.clone());
        s.connected.store(true, Ordering::SeqCst);
        self.log(&format!("IPC: connected to {}", s.endpoint));
        for command in mpv_ipc::observed::observe_commands() {
            self.send(&command);
        }
        (s.handlers.connected)();

        let mut reader = BufReader::with_capacity(16384, stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if s.disposed.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Some(message) = MpvIpcMessage::try_parse_line(&line) {
                        (s.handlers.message)(&message);
                    }
                }
                Err(e) => {
                    if !s.disposed.load(Ordering::SeqCst) {
                        self.log(&format!("IPC: read loop ended: {e}"));
                    }
                    break;
                }
            }
        }
        s.connected.store(false, Ordering::SeqCst);
        *s.writer.lock().unwrap() = None;
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::SeqCst)
    }

    /// Sends one JSON line; false when not connected or the write failed.
    pub fn send(&self, json_line: &str) -> bool {
        let mut guard = self.shared.writer.lock().unwrap();
        let Some(stream) = guard.as_mut() else { return false };
        if !self.is_connected() {
            return false;
        }
        let mut bytes = Vec::with_capacity(json_line.len() + 1);
        bytes.extend_from_slice(json_line.as_bytes());
        bytes.push(b'\n');
        match stream.write_all(&bytes) {
            Ok(()) => true,
            Err(e) => {
                drop(guard);
                self.log(&format!("IPC: send failed: {e}"));
                false
            }
        }
    }

    pub fn next_request_id(&self) -> i64 {
        self.shared.request_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Sends a command and returns the request id its reply will carry.
    pub fn command(&self, args: &[Value]) -> i64 {
        let id = self.next_request_id();
        self.send(&mpv_ipc::command(Some(id), args));
        id
    }

    pub fn set_property(&self, name: &str, value: impl Into<Value>) -> bool {
        self.send(&mpv_ipc::set_property(name, value, Some(self.next_request_id())))
    }

    pub fn cycle_property(&self, name: &str) -> bool {
        self.send(&mpv_ipc::cycle_property(name, Some(self.next_request_id())))
    }

    pub fn quit(&self) -> bool {
        self.send(&mpv_ipc::quit(Some(self.next_request_id())))
    }

    pub fn dispose(&self) {
        let s = &self.shared;
        if s.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        s.connected.store(false, Ordering::SeqCst);
        if let Some(stream) = s.writer.lock().unwrap().take() {
            stream.shutdown();
        }
    }
}
