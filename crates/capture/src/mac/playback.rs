//! AudioQueue output for control mode's sound (see `crate::playback`): 48 kHz stereo 16-bit linear PCM, three
//! 10 ms buffers that the queue's own thread asks us to refill. NOT YET RUN ON A MAC (type-checked on Windows only).

use std::ffi::c_void;
use std::sync::Mutex;

use crate::playback::PullFn;

const RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
const BUFFER_FRAMES: u32 = 480;
const BUFFERS: usize = 3;

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;
type OutputCallback = extern "C" fn(user: *mut c_void, queue: AudioQueueRef, buffer: AudioQueueBufferRef);

#[link(name = "AudioToolbox", kind = "framework")]
extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: OutputCallback,
        user: *mut c_void,
        run_loop: *const c_void,
        run_loop_mode: *const c_void,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: AudioQueueRef, bytes: u32, buffer: *mut AudioQueueBufferRef) -> i32;
    fn AudioQueueEnqueueBuffer(queue: AudioQueueRef, buffer: AudioQueueBufferRef, descriptions: u32, packets: *const c_void) -> i32;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> i32;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> i32;
}

/// What the callback reaches through its user pointer.
struct Source {
    pull: Mutex<PullFn>,
}

extern "C" fn refill(user: *mut c_void, queue: AudioQueueRef, buffer: AudioQueueBufferRef) {
    // SAFETY: `user` is the Source boxed by `start`, alive until the queue is disposed (in Drop, before the box);
    // `buffer` is one of the queue's own buffers, of BUFFER_FRAMES stereo i16 frames.
    unsafe {
        let source = &*(user as *const Source);
        let buffer = &mut *buffer;
        let samples = std::slice::from_raw_parts_mut(buffer.audio_data.cast::<i16>(), (BUFFER_FRAMES * CHANNELS) as usize);
        match source.pull.lock() {
            Ok(mut pull) => pull(samples),
            Err(_) => samples.fill(0),
        }
        buffer.audio_data_byte_size = BUFFER_FRAMES * CHANNELS * 2;
        AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null());
    }
}

pub(crate) struct Output {
    queue: AudioQueueRef,
    source: *mut Source,
}

// SAFETY: the queue is only started and disposed through this handle; its callback runs on the queue's own thread
// and touches only the Source, whose pull function is behind a mutex.
unsafe impl Send for Output {}

impl Output {
    pub(crate) fn start(pull: PullFn) -> Result<Output, String> {
        let format = AudioStreamBasicDescription {
            sample_rate: RATE,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: CHANNELS * 2,
            frames_per_packet: 1,
            bytes_per_frame: CHANNELS * 2,
            channels_per_frame: CHANNELS,
            bits_per_channel: 16,
            reserved: 0,
        };
        let source = Box::into_raw(Box::new(Source { pull: Mutex::new(pull) }));
        let mut queue: AudioQueueRef = std::ptr::null_mut();
        // SAFETY: valid format and out pointer; no run loop: the queue calls back on a thread of its own.
        let status = unsafe {
            AudioQueueNewOutput(&format, refill, source.cast(), std::ptr::null(), std::ptr::null(), 0, &mut queue)
        };
        if status != 0 || queue.is_null() {
            // SAFETY: the box was never handed to a queue.
            drop(unsafe { Box::from_raw(source) });
            return Err(format!("cannot open the sound output (AudioQueueNewOutput {status})"));
        }
        let output = Output { queue, source };
        for _ in 0..BUFFERS {
            let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
            // SAFETY: a live queue; the buffer it allocates is filled and handed back by `refill`.
            unsafe {
                if AudioQueueAllocateBuffer(queue, BUFFER_FRAMES * CHANNELS * 2, &mut buffer) != 0 || buffer.is_null() {
                    return Err("cannot allocate the sound buffers".to_owned());
                }
                refill(source.cast(), queue, buffer);
            }
        }
        // SAFETY: a live queue with its buffers enqueued.
        let status = unsafe { AudioQueueStart(queue, std::ptr::null()) };
        if status != 0 {
            return Err(format!("cannot start the sound output (AudioQueueStart {status})"));
        }
        Ok(output)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: the queue is stopped and disposed (no callback runs after that) before its Source is freed.
        unsafe {
            AudioQueueStop(self.queue, 1);
            AudioQueueDispose(self.queue, 1);
            drop(Box::from_raw(self.source));
        }
    }
}
