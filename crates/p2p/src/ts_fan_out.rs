//! Copies one MPEG-TS stream (the encoder's output) to many sinks (each viewer's remux child). Every sink has
//! its own bounded queue and writer thread, so a viewer whose child stalls — still waiting for its SRT
//! connection, or on a slow path — only loses its own data: when its queue overflows it is cleared and that
//! viewer's decoder recovers at the next keyframe. (Tested: a plain `tee` stalls every viewer at once.)

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::util::lock;

pub const READ_CHUNK_BYTES: usize = 188 * 7 * 32;
pub const MIN_QUEUE_BYTES: u64 = 512 * 1024;
/// No sink gets a queue smaller than this, whatever it asks for.
const SMALLEST_QUEUE_BYTES: u64 = 64 * 1024;

type InputEnded = Arc<dyn Fn() + Send + Sync>;
type SinkFailed = Arc<dyn Fn(&str) + Send + Sync>;
type QueueCleared = Arc<dyn Fn(&str) + Send + Sync>;

pub struct TsFanOut {
    shared: Arc<Shared>,
    input: Mutex<Option<Box<dyn Read + Send>>>,
}

struct Shared {
    sinks: Mutex<HashMap<String, Arc<Sink>>>,
    disposed: AtomicBool,
    bytes_in: AtomicU64,
    input_ended: Mutex<Option<InputEnded>>,
    sink_failed: Mutex<Option<SinkFailed>>,
    queue_cleared: Mutex<Option<QueueCleared>>,
}

struct Sink {
    id: String,
    max_bytes: u64,
    queue: Mutex<Queue>,
    wake: Condvar,
    written: AtomicU64,
}

#[derive(Default)]
struct Queue {
    chunks: VecDeque<Arc<[u8]>>,
    queued_bytes: u64,
    clears: u32,
    closed: bool,
}

impl Sink {
    /// True when the queue had to be cleared first.
    fn enqueue(&self, chunk: &Arc<[u8]>) -> bool {
        let mut queue = lock(&self.queue);
        if queue.closed {
            return false;
        }
        let cleared = queue.queued_bytes + chunk.len() as u64 > self.max_bytes;
        if cleared {
            queue.chunks.clear();
            queue.queued_bytes = 0;
            queue.clears += 1;
        }
        queue.chunks.push_back(chunk.clone());
        queue.queued_bytes += chunk.len() as u64;
        self.wake.notify_one();
        cleared
    }

    fn close(&self) {
        let mut queue = lock(&self.queue);
        queue.closed = true;
        queue.chunks.clear();
        queue.queued_bytes = 0;
        self.wake.notify_all();
    }
}

impl TsFanOut {
    pub fn new(input: Box<dyn Read + Send>) -> TsFanOut {
        TsFanOut {
            shared: Arc::new(Shared {
                sinks: Mutex::new(HashMap::new()),
                disposed: AtomicBool::new(false),
                bytes_in: AtomicU64::new(0),
                input_ended: Mutex::new(None),
                sink_failed: Mutex::new(None),
                queue_cleared: Mutex::new(None),
            }),
            input: Mutex::new(Some(input)),
        }
    }

    /// The encoder's stream ended (it exited or the pipe broke). Called on the reader thread; set before
    /// [`start`](Self::start).
    pub fn on_input_ended(&self, handler: impl Fn() + Send + Sync + 'static) {
        *lock(&self.shared.input_ended) = Some(Arc::new(handler));
    }

    /// A sink could not be written any more (its child exited); the sink is already removed. Called with its
    /// id on that sink's writer thread.
    pub fn on_sink_failed(&self, handler: impl Fn(&str) + Send + Sync + 'static) {
        *lock(&self.shared.sink_failed) = Some(Arc::new(handler));
    }

    /// A sink's queue overflowed and was emptied: its viewer lost what was in it. Called with its id on the
    /// reader thread (keep it short).
    pub fn on_queue_cleared(&self, handler: impl Fn(&str) + Send + Sync + 'static) {
        *lock(&self.shared.queue_cleared) = Some(Arc::new(handler));
    }

    pub fn bytes_in(&self) -> u64 {
        self.shared.bytes_in.load(Ordering::Relaxed)
    }

    /// Starts reading the input (once).
    pub fn start(&self) -> io::Result<()> {
        let Some(input) = lock(&self.input).take() else { return Ok(()) };
        let shared = self.shared.clone();
        thread::Builder::new().name("ts-fanout".to_string()).spawn(move || read_loop(&shared, input))?;
        Ok(())
    }

    /// Queue size for a stream of `bits_per_second`: half a second, at least 512 KB.
    pub fn queue_bytes_for(bits_per_second: u64) -> u64 {
        MIN_QUEUE_BYTES.max(bits_per_second / 8 / 2)
    }

    /// Adds a sink (replacing one with the same id). Its writer thread owns `output` and drops it when the
    /// sink is removed, fails, or the fan-out is disposed.
    pub fn add_sink(&self, id: &str, output: Box<dyn Write + Send>, max_queue_bytes: u64) -> io::Result<()> {
        let sink = Arc::new(Sink {
            id: id.to_string(),
            max_bytes: SMALLEST_QUEUE_BYTES.max(max_queue_bytes),
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            written: AtomicU64::new(0),
        });
        let (shared, writer_sink) = (self.shared.clone(), sink.clone());
        // Started before the sink is listed: a writer that cannot be started must not leave a queue nobody empties.
        thread::Builder::new().name(format!("ts-sink-{id}")).spawn(move || write_loop(&shared, &writer_sink, output))?;
        let old = lock(&self.shared.sinks).insert(id.to_string(), sink.clone());
        if let Some(old) = old {
            old.close();
        }
        if self.shared.disposed.load(Ordering::Acquire) {
            // disposed meanwhile: nobody will ever close this one
            lock(&self.shared.sinks).remove(id);
            sink.close();
        }
        Ok(())
    }

    pub fn remove_sink(&self, id: &str) {
        let sink = lock(&self.shared.sinks).remove(id);
        if let Some(sink) = sink {
            sink.close();
        }
    }

    /// (written bytes, times the queue overflowed and was cleared) for a sink, or `None`.
    pub fn stats(&self, id: &str) -> Option<(u64, u32)> {
        let sink = lock(&self.shared.sinks).get(id).cloned()?;
        let clears = lock(&sink.queue).clears;
        Some((sink.written.load(Ordering::Relaxed), clears))
    }

    pub fn sink_count(&self) -> usize {
        lock(&self.shared.sinks).len()
    }

    /// Stops feeding the sinks and lets go of them. The reader thread ends, and drops the input, when the
    /// read it is in returns: a blocking `Read` cannot be closed from here, so whoever owns the other end
    /// (the encoder's process, the [`SwitchableStream`](crate::SwitchableStream)) ends it.
    pub fn dispose(&self) {
        if self.shared.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        let sinks: Vec<Arc<Sink>> = lock(&self.shared.sinks).drain().map(|(_, sink)| sink).collect();
        for sink in sinks {
            sink.close();
        }
        lock(&self.input).take(); // never started: nothing reads it
    }
}

impl Drop for TsFanOut {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn read_loop(shared: &Shared, mut input: Box<dyn Read + Send>) {
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    while !shared.disposed.load(Ordering::Acquire) {
        let n = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        shared.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
        let chunk: Arc<[u8]> = Arc::from(&buffer[..n]);
        let sinks: Vec<Arc<Sink>> = lock(&shared.sinks).values().cloned().collect();
        for sink in sinks {
            if sink.enqueue(&chunk) {
                let handler = lock(&shared.queue_cleared).clone();
                if let Some(handler) = handler {
                    handler(&sink.id);
                }
            }
        }
    }
    drop(input);
    if !shared.disposed.load(Ordering::Acquire) {
        let handler = lock(&shared.input_ended).clone();
        if let Some(handler) = handler {
            handler();
        }
    }
}

fn write_loop(shared: &Shared, sink: &Arc<Sink>, mut output: Box<dyn Write + Send>) {
    loop {
        let chunk = {
            let mut queue = lock(&sink.queue);
            loop {
                if queue.closed {
                    return;
                }
                if let Some(chunk) = queue.chunks.pop_front() {
                    queue.queued_bytes -= chunk.len() as u64;
                    break chunk;
                }
                queue = sink.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        if output.write_all(&chunk).and_then(|()| output.flush()).is_err() {
            let mine = {
                let mut sinks = lock(&shared.sinks);
                let mine = sinks.get(&sink.id).is_some_and(|current| Arc::ptr_eq(current, sink));
                if mine {
                    sinks.remove(&sink.id);
                }
                mine
            };
            sink.close();
            if mine && !shared.disposed.load(Ordering::Acquire) {
                let handler = lock(&shared.sink_failed).clone();
                if let Some(handler) = handler {
                    handler(&sink.id);
                }
            }
            return;
        }
        sink.written.fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
}
