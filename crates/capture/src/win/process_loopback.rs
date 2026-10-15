//! Desktop audio without one process tree (design.md section 2.4): Windows' process-loopback capture, the API
//! behind Parsec's echo cancelling and OBS's application audio capture (Windows 10 21H1+ / 11). The virtual
//! endpoint has no mix format of its own, so the stream is requested as float 48 kHz stereo and the audio
//! engine converts.
//! Everything COM happens on one MTA thread per capture: on an STA the activation callback would need a
//! message pump nobody runs.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{implement, IUnknown, Interface, Ref, HRESULT, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use super::audio::BUFFER_DURATION_100NS;
use super::util::{error_text, hr_text, ComGuard, Handle};
use crate::log;
use crate::platform::AudioSink;
use crate::resample::{TARGET_CHANNELS, TARGET_SAMPLE_RATE};

const START_TIMEOUT: Duration = Duration::from_secs(8);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

fn text(e: windows::core::Error) -> String {
    error_text(&e)
}

/// Captures everything except (or only) one process tree, as f32 48 kHz stereo into the sink.
pub(crate) struct ProcessLoopbackCapture {
    target_pid: u32,
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProcessLoopbackCapture {
    /// Activates and starts the stream; fails when this Windows cannot (then use plain loopback).
    /// `include_tree`: true = only that process tree; false = everything except it.
    pub(crate) fn start(
        target_pid: u32,
        include_tree: bool,
        sink: AudioSink,
    ) -> Result<Self, String> {
        let running = Arc::new(AtomicBool::new(true));
        let (started_tx, started_rx) = mpsc::channel();
        let thread_running = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("process-loopback".into())
            .spawn(move || {
                let mut started = Some(started_tx);
                let result = run(
                    target_pid,
                    include_tree,
                    &sink,
                    &thread_running,
                    &mut started,
                );
                match (result, started) {
                    (Err(message), Some(started)) => {
                        let _ = started.send(Err(message));
                    }
                    (Err(message), None) => {
                        log::error(format!("audio: recording stopped: {message}"))
                    }
                    (Ok(()), _) => {}
                }
            })
            .map_err(|e| e.to_string())?;

        let outcome = started_rx
            .recv_timeout(START_TIMEOUT)
            .unwrap_or_else(|_| Err("process loopback did not start".to_owned()));
        match outcome {
            Ok(()) => Ok(Self {
                target_pid,
                running,
                thread: Some(thread),
            }),
            Err(message) => {
                running.store(false, Ordering::SeqCst);
                // A thread that reported its failure is about to end; one that hangs in the audio stack is left alone.
                if message != "process loopback did not start" {
                    let _ = thread.join();
                }
                Err(message)
            }
        }
    }

    pub(crate) fn target_pid(&self) -> u32 {
        self.target_pid
    }

    /// The stream format, the way the sender has always logged it.
    pub(crate) fn format_description(&self) -> String {
        format!("IeeeFloat {TARGET_SAMPLE_RATE} Hz {TARGET_CHANNELS} ch 32 bit")
    }
}

impl Drop for ProcessLoopbackCapture {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    target_pid: u32,
    include_tree: bool,
    sink: &AudioSink,
    running: &AtomicBool,
    started: &mut Option<Sender<Result<(), String>>>,
) -> Result<(), String> {
    // Declaration order is the reverse of the release order: capture client, audio client, event, COM.
    let _com = ComGuard::mta()?;
    // SAFETY: no attributes, no name; an auto-reset event that is ours to close.
    let ready =
        unsafe { CreateEventW(None, false, false, PCWSTR::null()).map(|h| Handle::from_raw(h)) }
            .map_err(text)?;
    let client = activate(target_pid, include_tree)?;

    let block_align = (TARGET_CHANNELS * 4) as u16;
    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: TARGET_CHANNELS as u16,
        nSamplesPerSec: TARGET_SAMPLE_RATE,
        nAvgBytesPerSec: TARGET_SAMPLE_RATE * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    // SAFETY: plain COM calls on a live interface; `format` and the event outlive the stream (the client is
    // released before the event is closed, see the declaration order).
    let capture: IAudioCaptureClient = unsafe {
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION_100NS, // 100 ms shared buffer
                0,
                &format,
                None,
            )
            .map_err(text)?;
        client.SetEventHandle(ready.raw()).map_err(text)?;
        let capture = client.GetService().map_err(text)?;
        client.Start().map_err(text)?;
        capture
    };
    if let Some(started) = started.take() {
        let _ = started.send(Ok(()));
    }

    let result = pump(&capture, &ready, sink, running);
    // SAFETY: plain COM call on a live interface.
    let _ = unsafe { client.Stop() };
    result
}

fn pump(
    capture: &IAudioCaptureClient,
    ready: &Handle,
    sink: &AudioSink,
    running: &AtomicBool,
) -> Result<(), String> {
    let mut samples: Vec<f32> = Vec::new();
    while running.load(Ordering::SeqCst) {
        // SAFETY: GetBuffer hands out `frames` frames (two f32 each, the format requested above) at `data`,
        // valid until the matching ReleaseBuffer, which follows on every path.
        unsafe {
            WaitForSingleObject(ready.raw(), 100);
            let mut pending = capture.GetNextPacketSize().map_err(text)?;
            while running.load(Ordering::SeqCst) && pending > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(text)?;
                let count = frames as usize * TARGET_CHANNELS;
                samples.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    samples.resize(count, 0.0);
                } else {
                    let bytes = std::slice::from_raw_parts(data, count * 4);
                    samples.extend(
                        bytes
                            .chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                    );
                }
                capture.ReleaseBuffer(frames).map_err(text)?;
                if !samples.is_empty() {
                    sink(&samples); // consumed synchronously
                }
                pending = capture.GetNextPacketSize().map_err(text)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- activation (audioclientactivationparams.h)

/// Signals the waiting thread when the activation finished. Objects made by `#[implement]` are agile, which
/// this handler must be: otherwise the callback waits for an apartment nobody pumps.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let (done, changed) = &*self.done;
        *done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        changed.notify_all();
        Ok(())
    }
}

/// The activation blob and the PROPVARIANT that points at it; the system reads both until the activation
/// completed.
struct ActivationRequest {
    params: AUDIOCLIENT_ACTIVATION_PARAMS,
    /// Never dropped: `PROPVARIANT`'s destructor is PropVariantClear, which would hand the blob - a pointer
    /// into this very struct - to CoTaskMemFree and corrupt the heap.
    prop: ManuallyDrop<PROPVARIANT>,
}

fn activate(target_pid: u32, include_tree: bool) -> Result<IAudioClient, String> {
    let mode = if include_tree {
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
    } else {
        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
    };
    let mut request = Box::new(ActivationRequest {
        params: AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: target_pid,
                    ProcessLoopbackMode: mode,
                },
            },
        },
        prop: ManuallyDrop::new(PROPVARIANT::default()),
    });
    let blob = BLOB {
        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        pBlobData: (&raw mut request.params).cast(),
    };
    // SAFETY: writes the VT_BLOB variant of a zero-initialised (VT_EMPTY) PROPVARIANT. The variant only
    // borrows the blob, which lives in the same heap block and therefore exactly as long as the variant.
    unsafe {
        let variant = &mut *request.prop.Anonymous.Anonymous;
        variant.vt = VT_BLOB;
        variant.Anonymous.blob = blob;
    }

    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
        done: Arc::clone(&done),
    }
    .into();
    // SAFETY: the device path is a system constant, the IID a static, and `request` stays allocated until
    // the activation completed (or forever, see below).
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*request.prop),
            &handler,
        )
    }
    .map_err(text)?;

    // The blob has to stay valid until the activation completes, so wait here before freeing it.
    let (flag, changed) = &*done;
    let (completed, _) = changed
        .wait_timeout_while(
            flag.lock().unwrap_or_else(PoisonError::into_inner),
            ACTIVATION_TIMEOUT,
            |completed| !*completed,
        )
        .unwrap_or_else(PoisonError::into_inner);
    if !*completed {
        // The system may still read the request whenever it gets around to it: leak the few bytes.
        std::mem::forget(request);
        return Err("process loopback activation did not complete".to_owned());
    }
    drop(completed);

    let mut activate_result: HRESULT = E_FAIL;
    let mut activated: Option<IUnknown> = None;
    // SAFETY: plain COM call; both out pointers are valid locals.
    unsafe { operation.GetActivateResult(&mut activate_result, &mut activated) }.map_err(text)?;
    if activate_result.is_err() {
        let error = windows::core::Error::from_hresult(activate_result);
        return Err(format!(
            "{} (HRESULT {})",
            error.message().trim(),
            hr_text(activate_result)
        ));
    }
    activated
        .and_then(|unknown| unknown.cast::<IAudioClient>().ok())
        .ok_or_else(|| "process loopback returned no audio client".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Activation, start and teardown, several times over. The request block (a PROPVARIANT whose blob
    /// points into our own memory) must be released by us alone: letting PROPVARIANT's destructor clear it
    /// corrupted the heap on the very first activation.
    #[test]
    fn process_loopback_starts_and_stops_cleanly() {
        let sink: AudioSink =
            Arc::new(|samples: &[f32]| assert_eq!(samples.len() % TARGET_CHANNELS, 0));
        for round in 0..3 {
            match ProcessLoopbackCapture::start(std::process::id(), false, Arc::clone(&sink)) {
                Ok(capture) => {
                    assert_eq!(capture.target_pid(), std::process::id());
                    assert_eq!(
                        capture.format_description(),
                        "IeeeFloat 48000 Hz 2 ch 32 bit"
                    );
                    std::thread::sleep(Duration::from_millis(150));
                }
                // Before Windows 10 21H1, or without an audio service: the engine falls back to plain loopback.
                Err(message) => {
                    eprintln!("note: process loopback is not available here ({message}); round {round} skipped");
                    return;
                }
            }
        }
    }
}
