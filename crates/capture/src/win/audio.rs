//! Desktop audio via WASAPI loopback (design.md section 2.4). With an app to leave out (the voice chat) it is
//! process loopback instead ([`ProcessLoopbackCapture`]), following that app as it starts, quits and
//! restarts. Whatever is captured arrives in the sink as f32 48 kHz stereo.
//!
//! COM: every thread that touches WASAPI enters the multithreaded apartment itself and creates its own
//! objects ([`ComGuard`]); no COM pointer crosses a thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{Interface, GUID};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;

use super::process::take_snapshot;
use super::process_loopback::ProcessLoopbackCapture;
use super::util::{error_text, run_mta, take_co_string, ComGuard};
use crate::app_audio;
use crate::log;
use crate::platform::{AudioDeviceInfo, AudioSink, AudioSource};
use crate::resample::{PcmFormat, StereoConverter};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Shared-mode buffer of the capture and keep-alive streams: 100 ms (in 100 ns units).
pub(crate) const BUFFER_DURATION_100NS: i64 = 1_000_000;
/// How often the loopback buffer is emptied. The endpoint delivers a packet per 10 ms device period.
const CAPTURE_POLL: Duration = Duration::from_millis(10);
/// How often the keep-alive stream is topped up (half its buffer).
const KEEP_ALIVE_POLL: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(8);

fn text(e: windows::core::Error) -> String {
    error_text(&e)
}

/// A WAVEFORMATEX as the pieces the engine needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WaveFormat {
    /// The format tag, with WAVE_FORMAT_EXTENSIBLE resolved to the plain tag of its sub-format where there
    /// is one (float, PCM).
    pub tag: u16,
    pub pcm: PcmFormat,
    pub block_align: usize,
}

impl WaveFormat {
    /// # Safety
    /// `format` must point to a valid WAVEFORMATEX followed by its `cbSize` extra bytes (as the audio engine
    /// returns it).
    pub(crate) unsafe fn read(format: *const WAVEFORMATEX) -> Self {
        // SAFETY: valid per the contract; the struct is packed, hence the unaligned reads.
        let f = unsafe { format.read_unaligned() };
        let (mut tag, bits) = (f.wFormatTag, f.wBitsPerSample);
        // The shared-mode mix format is IEEE float; WAVE_FORMAT_EXTENSIBLE wraps the same thing.
        if tag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
            // SAFETY: an extensible format with cbSize >= 22 is a whole WAVEFORMATEXTENSIBLE.
            let ext = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            let sub_format = ext.SubFormat;
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                tag = WAVE_FORMAT_IEEE_FLOAT;
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                tag = WAVE_FORMAT_PCM;
            }
        }
        Self {
            tag,
            pcm: PcmFormat {
                sample_rate: f.nSamplesPerSec,
                channels: f.nChannels,
                bits_per_sample: bits,
                float: tag == WAVE_FORMAT_IEEE_FLOAT,
            },
            block_align: usize::from(f.nBlockAlign),
        }
    }

    /// `IeeeFloat 48000 Hz 2 ch 32 bit`, the way the sender has always logged it.
    pub(crate) fn describe(&self) -> String {
        let encoding = match self.tag {
            WAVE_FORMAT_PCM => "Pcm".to_owned(),
            WAVE_FORMAT_IEEE_FLOAT => "IeeeFloat".to_owned(),
            WAVE_FORMAT_EXTENSIBLE => "Extensible".to_owned(),
            other => other.to_string(),
        };
        format!(
            "{encoding} {} Hz {} ch {} bit",
            self.pcm.sample_rate, self.pcm.channels, self.pcm.bits_per_sample
        )
    }
}

/// The endpoint's mix format, freed with `CoTaskMemFree` on drop.
struct MixFormat(*mut WAVEFORMATEX);

impl MixFormat {
    fn of(client: &IAudioClient) -> Result<Self, String> {
        // SAFETY: plain COM call; on success the returned block is ours to free.
        let format = unsafe { client.GetMixFormat() }.map_err(text)?;
        if format.is_null() {
            return Err("the audio endpoint has no mix format".to_owned());
        }
        Ok(Self(format))
    }

    fn wave_format(&self) -> WaveFormat {
        // SAFETY: the pointer came from GetMixFormat and is alive until drop.
        unsafe { WaveFormat::read(self.0) }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by the audio engine with CoTaskMemAlloc, freed exactly once.
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast())) };
    }
}

fn device_enumerator() -> Result<IMMDeviceEnumerator, String> {
    // SAFETY: plain COM activation on a thread inside an apartment (the callers hold a ComGuard).
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(text)
}

fn device_id(device: &IMMDevice) -> Result<String, String> {
    // SAFETY: GetId returns a CoTaskMem string that take_co_string frees.
    unsafe { device.GetId().map(|id| take_co_string(id)) }.map_err(text)
}

fn active_render_devices(enumerator: &IMMDeviceEnumerator) -> Result<Vec<IMMDevice>, String> {
    // SAFETY: plain COM calls on live interfaces.
    unsafe {
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(text)?;
        let count = collection.GetCount().map_err(text)?;
        (0..count)
            .map(|i| collection.Item(i).map_err(text))
            .collect()
    }
}

/// "default" (or nothing) is the default render endpoint; anything else an endpoint id.
fn resolve_device(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<IMMDevice, String> {
    if id.is_empty() || id == "default" {
        // SAFETY: plain COM call on a live interface.
        return unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(text);
    }
    for device in active_render_devices(enumerator)? {
        if device_id(&device).is_ok_and(|candidate| candidate.eq_ignore_ascii_case(id)) {
            return Ok(device);
        }
    }
    Err(format!("audio device not found: {id}"))
}

fn friendly_name(device: &IMMDevice) -> Option<String> {
    // SAFETY: plain COM calls; the PROPVARIANT returned by GetValue is only read as the type its tag
    // announces, and only while it is alive (it clears itself when dropped).
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        if value.vt() != VT_LPWSTR {
            return None;
        }
        let text = value.Anonymous.Anonymous.Anonymous.pwszVal;
        (!text.is_null()).then(|| String::from_utf16_lossy(text.as_wide()))
    }
}

/// Lists the active render endpoints for the app.
pub(crate) fn list_devices() -> Vec<AudioDeviceInfo> {
    let listed = run_mta(|| {
        let enumerator = device_enumerator()?;
        let default_id = device_id(&resolve_device(&enumerator, "default")?)?;
        let mut list = Vec::new();
        for device in active_render_devices(&enumerator)? {
            let id = device_id(&device)?;
            list.push(AudioDeviceInfo {
                is_default: id == default_id,
                name: friendly_name(&device).unwrap_or_default(),
                id,
            });
        }
        Ok(list)
    });
    listed.unwrap_or_else(|message| {
        log::warn(format!("audio: cannot enumerate devices: {message}"));
        Vec::new()
    })
}

/// Programs that have an audio session on the endpoint, as exe names, for the "Leave out" list.
pub(crate) fn list_audio_apps(device: &str) -> Vec<String> {
    let session_pids = run_mta(|| {
        let enumerator = device_enumerator()?;
        let device = resolve_device(&enumerator, device)?;
        let mut pids = Vec::new();
        // SAFETY: plain COM calls on live interfaces.
        unsafe {
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).map_err(text)?;
            let sessions = manager.GetSessionEnumerator().map_err(text)?;
            for i in 0..sessions.GetCount().map_err(text)? {
                let Ok(control) = sessions
                    .GetSession(i)
                    .and_then(|s| s.cast::<IAudioSessionControl2>())
                else {
                    continue;
                };
                if control.IsSystemSoundsSession() == S_OK {
                    continue;
                }
                match control.GetProcessId() {
                    Ok(pid) if pid > 0 => pids.push(pid),
                    _ => {}
                }
            }
        }
        Ok(pids)
    });
    let session_pids = session_pids.unwrap_or_else(|message| {
        log::warn(format!("audio: cannot list audio sessions: {message}"));
        Vec::new()
    });

    let mut names: Vec<String> = Vec::new();
    if !session_pids.is_empty() {
        // gone meanwhile = simply not in the snapshot
        for process in take_snapshot().unwrap_or_default() {
            if session_pids.contains(&process.pid) && !process.exe_name.is_empty() {
                names.push(process.exe_name);
            }
        }
    }
    let own = std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_lowercase())
    });
    names.retain(|name| Some(name.to_lowercase()) != own);
    names.sort_by_key(|name| name.to_uppercase());
    names.dedup_by_key(|name| name.to_uppercase());
    names
}

/// A capture or render thread with a stop flag; stopped and joined on drop.
struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Runs `body(stop, started)` on a thread named `name` and waits until it reported through `started`
    /// that the stream runs (or why it does not).
    fn start<T: Send + 'static>(
        name: &str,
        body: impl FnOnce(Arc<AtomicBool>, Sender<Result<T, String>>) + Send + 'static,
    ) -> Result<(Self, T), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || body(thread_stop, started_tx))
            .map_err(|e| e.to_string())?;
        let mut worker = Self {
            stop,
            thread: Some(thread),
        };
        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(value)) => Ok((worker, value)),
            Ok(Err(message)) => Err(message),
            Err(_) => {
                // Hung inside the audio stack: do not wait for it (the thread ends on its own if it ever returns).
                worker.stop.store(true, Ordering::SeqCst);
                worker.thread = None;
                Err(format!("{name} did not start"))
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reports the outcome of a worker body: a failure before the stream ran goes to whoever waits in
/// [`Worker::start`], a failure afterwards to the log.
fn report(result: Result<(), String>, started: Option<Sender<Result<String, String>>>) {
    match (result, started) {
        (Err(message), Some(started)) => {
            let _ = started.send(Err(message));
        }
        (Err(message), None) => log::error(format!("audio: recording stopped: {message}")),
        (Ok(()), _) => {}
    }
}

/// Plain loopback of the whole endpoint, polled.
fn device_loopback(device_id: &str, sink: AudioSink) -> Result<(Worker, String), String> {
    let device_id = device_id.to_owned();
    Worker::start("wasapi-loopback", move |stop, started| {
        let mut started = Some(started);
        let result = run_device_loopback(&device_id, &sink, &stop, &mut started);
        report(result, started);
    })
}

fn run_device_loopback(
    device_id: &str,
    sink: &AudioSink,
    stop: &AtomicBool,
    started: &mut Option<Sender<Result<String, String>>>,
) -> Result<(), String> {
    let _com = ComGuard::mta()?;
    let device = resolve_device(&device_enumerator()?, device_id)?;
    // SAFETY: plain COM activation on a live interface.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(text)?;
    let mix = MixFormat::of(&client)?;
    let format = mix.wave_format();
    // SAFETY: plain COM calls on live interfaces; `mix` is alive across Initialize.
    let capture: IAudioCaptureClient = unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_100NS,
                0,
                mix.0,
                None,
            )
            .map_err(text)?;
        let capture = client.GetService().map_err(text)?;
        client.Start().map_err(text)?;
        capture
    };
    if let Some(started) = started.take() {
        let _ = started.send(Ok(format.describe()));
    }

    let mut converter = StereoConverter::new(format.pcm);
    let mut chunk: Vec<f32> = Vec::new();
    let mut silence: Vec<u8> = Vec::new();
    let result = (|| -> Result<(), String> {
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(CAPTURE_POLL);
            chunk.clear();
            // SAFETY: GetBuffer hands out `frames` frames of `block_align` bytes at `data`, valid until the
            // matching ReleaseBuffer, which follows on every path (the conversion in between cannot fail).
            unsafe {
                while capture.GetNextPacketSize().map_err(text)? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                    capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .map_err(text)?;
                    let bytes = frames as usize * format.block_align;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        silence.resize(bytes, 0);
                        converter.convert(&silence[..bytes], &mut chunk);
                    } else {
                        converter.convert(std::slice::from_raw_parts(data, bytes), &mut chunk);
                    }
                    capture.ReleaseBuffer(frames).map_err(text)?;
                }
            }
            if !chunk.is_empty() {
                sink(&chunk);
            }
        }
        Ok(())
    })();
    // SAFETY: plain COM call on a live interface.
    let _ = unsafe { client.Stop() };
    result
}

/// A completely idle render endpoint delivers no loopback data at all, so play silence into it.
fn silence_keep_alive(device_id: &str) -> Result<Worker, String> {
    let device_id = device_id.to_owned();
    Worker::start("wasapi-silence", move |stop, started| {
        let mut started = Some(started);
        // A keep-alive that stops later (device unplugged) is not worth a log line: the capture reports it.
        if let (Err(message), Some(started)) =
            (run_silence(&device_id, &stop, &mut started), started)
        {
            let _ = started.send(Err(message));
        }
    })
    .map(|(worker, ())| worker)
}

fn run_silence(
    device_id: &str,
    stop: &AtomicBool,
    started: &mut Option<Sender<Result<(), String>>>,
) -> Result<(), String> {
    let _com = ComGuard::mta()?;
    let device = resolve_device(&device_enumerator()?, device_id)?;
    // SAFETY: plain COM activation on a live interface.
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(text)?;
    let mix = MixFormat::of(&client)?;
    // SAFETY: plain COM calls on live interfaces; `mix` is alive across Initialize.
    let (render, buffer_frames): (IAudioRenderClient, u32) = unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                BUFFER_DURATION_100NS,
                0,
                mix.0,
                None,
            )
            .map_err(text)?;
        (
            client.GetService().map_err(text)?,
            client.GetBufferSize().map_err(text)?,
        )
    };
    let fill = || -> Result<(), String> {
        // SAFETY: the frames obtained from GetBuffer are released at once, flagged silent (so their
        // content is never read by us nor played as garbage).
        unsafe {
            let available = buffer_frames.saturating_sub(client.GetCurrentPadding().map_err(text)?);
            if available > 0 {
                render.GetBuffer(available).map_err(text)?;
                render
                    .ReleaseBuffer(available, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .map_err(text)?;
            }
        }
        Ok(())
    };
    fill()?;
    // SAFETY: plain COM call on a live interface.
    unsafe { client.Start() }.map_err(text)?;
    if let Some(started) = started.take() {
        let _ = started.send(Ok(()));
    }
    let mut result = Ok(());
    while !stop.load(Ordering::SeqCst) && result.is_ok() {
        std::thread::sleep(KEEP_ALIVE_POLL);
        result = fill();
    }
    // SAFETY: plain COM call on a live interface.
    let _ = unsafe { client.Stop() };
    result
}

/// The capture that currently feeds the sink. Guarded by one mutex: switching (the excluded app restarted)
/// replaces it as a whole.
#[derive(Default)]
struct Captures {
    process: Option<ProcessLoopbackCapture>,
    device: Option<Worker>,
    description: String,
}

struct Shared {
    captures: Mutex<Captures>,
    /// Set when the source is dropped; wakes the follower.
    stopped: Mutex<bool>,
    stopped_changed: Condvar,
    sink: AudioSink,
    device_id: String,
    exclude_app: String,
    self_pid: u32,
}

impl Shared {
    fn lock_captures(&self) -> std::sync::MutexGuard<'_, Captures> {
        self.captures.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Plain loopback of the whole endpoint.
    fn start_device_capture(&self, captures: &mut Captures) -> Result<(), String> {
        let (worker, description) = device_loopback(&self.device_id, Arc::clone(&self.sink))?;
        captures.description = description;
        captures.device = Some(worker);
        Ok(())
    }

    /// Everything except `target`'s process tree (the excluded app, or this engine while that app is not
    /// running). False when this Windows has no process loopback.
    fn start_process_capture(&self, captures: &mut Captures, target: u32, root: u32) -> bool {
        let capture = match ProcessLoopbackCapture::start(target, false, Arc::clone(&self.sink)) {
            Ok(capture) => capture,
            Err(message) => {
                log::warn(format!("audio: process loopback unavailable: {message}"));
                return false;
            }
        };
        captures.description = format!(
            "{}, without {}",
            capture.format_description(),
            self.exclude_app
        );
        captures.process = Some(capture);
        let app = &self.exclude_app;
        log::info(if root > 0 {
            format!("audio: leaving out {app} (pid {root} and everything it started)")
        } else {
            format!("audio: {app} is not running; sending all desktop audio until it starts")
        });
        true
    }

    /// The process tree to exclude: the app's topmost process, or this engine (i.e. nothing) while it is not
    /// running. Returns `(target, root)`; `root` is 0 while the app is not running.
    fn exclude_target(&self) -> (u32, u32) {
        let root = match take_snapshot() {
            Ok(processes) => app_audio::find_root(&processes, &self.exclude_app),
            Err(message) => {
                log::warn(format!("audio: cannot list processes: {message}"));
                0
            }
        };
        (if root > 0 { root } else { self.self_pid }, root)
    }

    /// Every second (a process snapshot takes a millisecond or two): when the app started, quit or restarted
    /// the excluded tree changed, so restart the capture on it. Until then up to a second of a freshly
    /// started app can slip through.
    fn follow_excluded_app(&self) {
        loop {
            let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
            let (stopped, _) = self
                .stopped_changed
                .wait_timeout_while(stopped, Duration::from_secs(1), |stopped| !*stopped)
                .unwrap_or_else(PoisonError::into_inner);
            if *stopped {
                return;
            }
            drop(stopped);

            let mut captures = self.lock_captures();
            let Some(current) = captures.process.as_ref() else {
                return;
            };
            let (target, root) = self.exclude_target();
            if target == current.target_pid() {
                continue;
            }
            captures.process = None; // stops the old capture before the new one starts
            if self.start_process_capture(&mut captures, target, root) {
                continue;
            }
            log::warn("audio: sending all desktop audio for the rest of this session");
            if let Err(message) = self.start_device_capture(&mut captures) {
                log::error(format!("audio: cannot restart the capture: {message}"));
            }
            return;
        }
    }
}

/// The Windows [`AudioSource`].
pub(crate) struct WasapiAudioSource {
    shared: Arc<Shared>,
    follower: Option<JoinHandle<()>>,
    _silence: Option<Worker>,
}

/// Starts capturing `device` into `sink`, leaving out `exclude_app` when given. An error means there is no
/// desktop audio at all (the caller then sends silence).
pub(crate) fn open(
    device: &str,
    exclude_app: &str,
    sink: AudioSink,
) -> Result<WasapiAudioSource, String> {
    // Resolve the endpoint first: a stale id must fail here, whichever way the audio is then captured.
    let (device_id, device_name) = run_mta(|| {
        let endpoint = resolve_device(&device_enumerator()?, device)?;
        Ok((device_id(&endpoint)?, friendly_name(&endpoint)))
    })?;

    let shared = Arc::new(Shared {
        captures: Mutex::new(Captures::default()),
        stopped: Mutex::new(false),
        stopped_changed: Condvar::new(),
        sink,
        device_id,
        exclude_app: exclude_app.to_owned(),
        self_pid: std::process::id(),
    });

    let mut follower = None;
    let description = {
        let mut captures = shared.lock_captures();
        if !exclude_app.is_empty() {
            let (target, root) = shared.exclude_target();
            if shared.start_process_capture(&mut captures, target, root) {
                let follower_shared = Arc::clone(&shared);
                follower = std::thread::Builder::new()
                    .name("audio-exclude-follower".into())
                    .spawn(move || follower_shared.follow_excluded_app())
                    .ok();
            } else {
                log::warn(format!(
                    "audio: cannot leave out {exclude_app} on this Windows (needs Windows 10 21H1 or newer); sending all desktop audio"
                ));
            }
        }
        if captures.process.is_none() {
            shared.start_device_capture(&mut captures)?;
        }
        captures.description.clone()
    };

    let silence = match silence_keep_alive(&shared.device_id) {
        Ok(worker) => Some(worker),
        Err(message) => {
            log::warn(format!(
                "audio: could not start the silence keep-alive: {message}"
            ));
            None
        }
    };

    log::info(format!(
        "audio: capturing {description} from {}",
        device_name.as_deref().unwrap_or("default")
    ));
    Ok(WasapiAudioSource {
        shared,
        follower,
        _silence: silence,
    })
}

impl AudioSource for WasapiAudioSource {
    fn description(&self) -> String {
        self.shared.lock_captures().description.clone()
    }
}

impl Drop for WasapiAudioSource {
    fn drop(&mut self) {
        *self
            .shared
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = true;
        self.shared.stopped_changed.notify_all();
        if let Some(follower) = self.follower.take() {
            let _ = follower.join();
        }
        let mut captures = self.shared.lock_captures();
        captures.process = None;
        captures.device = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TONE_HZ: f64 = 1000.0;
    /// -90 dBFS: far below anything a speaker makes audible.
    const TONE_AMPLITUDE: f32 = 3.16e-5;

    /// Plays a 1 kHz sine into the default endpoint until dropped. `None` when the mix format is not float.
    fn play_tone() -> Option<Worker> {
        let started = Worker::start("test-tone", move |stop, started| {
            let mut started = Some(started);
            let result = (|| -> Result<(), String> {
                let _com = ComGuard::mta()?;
                let device = resolve_device(&device_enumerator()?, "default")?;
                // SAFETY: the same calls as the keep-alive stream; the render buffer is filled for exactly the
                // frames GetBuffer handed out, in the channel count of the mix format.
                unsafe {
                    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(text)?;
                    let mix = MixFormat::of(&client)?;
                    let format = mix.wave_format();
                    if !format.pcm.float || format.pcm.bits_per_sample != 32 {
                        return Err("the mix format is not 32-bit float".to_owned());
                    }
                    client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            0,
                            BUFFER_DURATION_100NS,
                            0,
                            mix.0,
                            None,
                        )
                        .map_err(text)?;
                    let render: IAudioRenderClient = client.GetService().map_err(text)?;
                    let buffer_frames = client.GetBufferSize().map_err(text)?;
                    let channels = usize::from(format.pcm.channels);
                    let step =
                        2.0 * std::f64::consts::PI * TONE_HZ / f64::from(format.pcm.sample_rate);
                    let mut phase = 0f64;
                    let mut fill = || -> Result<(), String> {
                        let available =
                            buffer_frames.saturating_sub(client.GetCurrentPadding().map_err(text)?);
                        if available == 0 {
                            return Ok(());
                        }
                        let data = render.GetBuffer(available).map_err(text)?;
                        let samples = std::slice::from_raw_parts_mut(
                            data.cast::<f32>(),
                            available as usize * channels,
                        );
                        for frame in samples.chunks_exact_mut(channels) {
                            frame.fill(TONE_AMPLITUDE * phase.sin() as f32);
                            phase += step;
                        }
                        render.ReleaseBuffer(available, 0).map_err(text)
                    };
                    fill()?;
                    client.Start().map_err(text)?;
                    if let Some(started) = started.take() {
                        let _ = started.send(Ok(()));
                    }
                    while !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                        fill()?;
                    }
                    let _ = client.Stop();
                }
                Ok(())
            })();
            if let (Err(message), Some(started)) = (result, started) {
                let _ = started.send(Err(message));
            }
        });
        match started {
            Ok((worker, ())) => Some(worker),
            Err(message) => {
                eprintln!("note: cannot play the test tone ({message})");
                None
            }
        }
    }

    /// Level in dBFS and frequency in Hz (from zero crossings) of the left channel.
    fn measure(samples: &[f32]) -> (f64, f64) {
        let left: Vec<f64> = samples
            .chunks_exact(2)
            .map(|frame| f64::from(frame[0]))
            .collect();
        let rms = (left.iter().map(|v| v * v).sum::<f64>() / left.len().max(1) as f64).sqrt();
        let crossings = left
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let seconds = left.len() as f64 / 48000.0;
        (
            20.0 * rms.max(1e-12).log10(),
            crossings as f64 / 2.0 / seconds.max(1e-9),
        )
    }

    /// Runs `start` with a sink that collects, for half a second (after a moment to settle).
    fn record<C>(start: impl FnOnce(AudioSink) -> Result<C, String>) -> Vec<f32> {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink_collected = Arc::clone(&collected);
        let sink: AudioSink = Arc::new(move |samples: &[f32]| {
            sink_collected.lock().unwrap().extend_from_slice(samples)
        });
        let capture = start(sink).expect("the capture starts");
        std::thread::sleep(Duration::from_millis(300));
        collected.lock().unwrap().clear();
        std::thread::sleep(Duration::from_millis(500));
        drop(capture);
        let samples = collected.lock().unwrap().clone();
        samples
    }

    /// End to end with real sound: what is rendered on the endpoint arrives in the sink, in the right format,
    /// and process loopback really leaves a process tree out. Opt-in (`cargo test -- --ignored`) because it
    /// plays a tone - at -90 dBFS, so nobody hears it - and needs the endpoint to be otherwise silent.
    #[test]
    #[ignore = "plays an inaudible (-90 dBFS) tone on the default endpoint; needs it unmuted and otherwise silent"]
    fn loopback_hears_a_tone_and_process_loopback_can_leave_this_process_out() {
        let Some(_tone) = play_tone() else { return };
        let device =
            run_mta(|| device_id(&resolve_device(&device_enumerator()?, "default")?)).unwrap();
        let own_pid = std::process::id();

        // Plain loopback: the whole endpoint, our tone included.
        let (level, frequency) = measure(&record(|sink| device_loopback(&device, sink)));
        eprintln!("endpoint loopback: {level:.1} dBFS at {frequency:.0} Hz");
        assert!(
            (-130.0..=-60.0).contains(&level),
            "the tone was played at -90 dBFS, captured at {level:.1} dBFS"
        );
        assert!(
            (TONE_HZ * 0.95..=TONE_HZ * 1.05).contains(&frequency),
            "{frequency:.0} Hz"
        );

        // Everything except this process (what the engine does for the app to leave out): the tone is gone.
        let (without, _) = measure(&record(|sink| {
            ProcessLoopbackCapture::start(own_pid, false, sink)
        }));
        eprintln!("without this process: {without:.1} dBFS");
        assert!(
            without < level - 30.0,
            "the tone is still there at {without:.1} dBFS (endpoint: {level:.1} dBFS)"
        );

        // Only this process: the tone again.
        let (only, frequency) = measure(&record(|sink| {
            ProcessLoopbackCapture::start(own_pid, true, sink)
        }));
        eprintln!("only this process: {only:.1} dBFS at {frequency:.0} Hz");
        assert!(
            (only - level).abs() < 12.0,
            "{only:.1} dBFS (endpoint: {level:.1} dBFS)"
        );
        assert!(
            (TONE_HZ * 0.95..=TONE_HZ * 1.05).contains(&frequency),
            "{frequency:.0} Hz"
        );
    }

    #[test]
    fn wave_formats_are_described_the_way_the_log_always_showed_them() {
        let extensible = |sub_format: GUID, bits: u16| WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 48000 * 2 * u32::from(bits / 8),
                nBlockAlign: 2 * bits / 8,
                wBitsPerSample: bits,
                cbSize: 22,
            },
            SubFormat: sub_format,
            ..Default::default()
        };
        let ext = extensible(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, 32);
        // SAFETY: a whole WAVEFORMATEXTENSIBLE, as its cbSize says.
        let format = unsafe { WaveFormat::read((&raw const ext).cast()) };
        assert_eq!(format.describe(), "IeeeFloat 48000 Hz 2 ch 32 bit");
        assert!(format.pcm.float);
        assert_eq!(format.block_align, 8);

        // The PCM sub-format (16-bit endpoints, some virtual cables) is integer.
        let ext = extensible(KSDATAFORMAT_SUBTYPE_PCM, 16);
        // SAFETY: as above.
        let format = unsafe { WaveFormat::read((&raw const ext).cast()) };
        assert_eq!(format.describe(), "Pcm 48000 Hz 2 ch 16 bit");
        assert!(!format.pcm.float);
        assert_eq!(format.pcm.bytes_per_frame(), 4);

        // A sub-format the engine does not know keeps its tag (and decodes as integer PCM).
        let ext = extensible(GUID::from_u128(0x00000092_0000_0010_8000_00aa00389b71), 16);
        // SAFETY: as above.
        let format = unsafe { WaveFormat::read((&raw const ext).cast()) };
        assert_eq!(format.describe(), "Extensible 48000 Hz 2 ch 16 bit");

        let plain = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: 6,
            nSamplesPerSec: 44100,
            nAvgBytesPerSec: 0,
            nBlockAlign: 24,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // SAFETY: a plain WAVEFORMATEX with no extra bytes.
        let format = unsafe { WaveFormat::read(&plain) };
        assert_eq!(format.describe(), "IeeeFloat 44100 Hz 6 ch 32 bit");
        assert!(format.pcm.float);
    }
}
