//! WASAPI output for control mode's sound (see `crate::playback`): shared mode on the default render endpoint, our
//! 48 kHz stereo 16-bit converted by Windows (AUTOCONVERTPCM). The device buffer is topped up every 2 ms to about
//! [`QUEUED_MS`]: two of the audio engine's 10 ms periods, so it never runs dry, and no more, so the sound is not
//! held back. When the endpoint goes away (unplugged, the default changed) the default is opened again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::util::{error_text, ComGuard};
use crate::log;
use crate::playback::PullFn;

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// What is kept queued at the device.
const QUEUED_MS: u32 = 20;
/// The shared buffer asked for (100 ns units): room above what is kept queued.
const BUFFER_100NS: i64 = 400_000;
const TOP_UP: Duration = Duration::from_millis(2);
const REOPEN_AFTER: Duration = Duration::from_millis(500);

pub(crate) struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    pub(crate) fn start(mut pull: PullFn) -> Result<Output, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("sound-output".into())
            .spawn(move || {
                let mut started = Some(started_tx);
                while !thread_stop.load(Ordering::SeqCst) {
                    let result = play(&mut pull, &thread_stop, &mut started);
                    if let Err(message) = result {
                        match started.take() {
                            Some(started) => {
                                let _ = started.send(Err(message));
                                return;
                            }
                            None => log::warn(format!("sound: {message}; opening the default output again")),
                        }
                    }
                    std::thread::sleep(REOPEN_AFTER);
                }
            })
            .map_err(|e| e.to_string())?;
        match started_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Output { stop, thread: Some(thread) }),
            Ok(Err(message)) => Err(message),
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                Err("the sound output did not start".to_owned())
            }
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Plays on the default output until told to stop (Ok) or the endpoint fails (Err).
fn play(pull: &mut PullFn, stop: &AtomicBool, started: &mut Option<std::sync::mpsc::Sender<Result<(), String>>>) -> Result<(), String> {
    let _com = ComGuard::mta()?;
    let block_align = CHANNELS * 2;
    let format = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: CHANNELS,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    // SAFETY: plain COM calls on live interfaces; `format` outlives Initialize.
    let (client, render, buffer_frames): (IAudioClient, IAudioRenderClient, u32) = unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| error_text(&e))?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| format!("no sound output: {}", error_text(&e)))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| error_text(&e))?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_100NS,
                0,
                &format,
                None,
            )
            .map_err(|e| error_text(&e))?;
        let render = client.GetService().map_err(|e| error_text(&e))?;
        let frames = client.GetBufferSize().map_err(|e| error_text(&e))?;
        (client, render, frames)
    };
    let queued = (RATE * QUEUED_MS / 1000).min(buffer_frames);
    let mut top_up = || -> Result<(), String> {
        // SAFETY: the frames GetBuffer hands out are filled completely (2 channels of i16 each) and released at once.
        unsafe {
            let padding = client.GetCurrentPadding().map_err(|e| error_text(&e))?;
            let want = queued.saturating_sub(padding);
            if want > 0 {
                let data = render.GetBuffer(want).map_err(|e| error_text(&e))?;
                let samples = std::slice::from_raw_parts_mut(data.cast::<i16>(), want as usize * usize::from(CHANNELS));
                pull(samples);
                render.ReleaseBuffer(want, 0).map_err(|e| error_text(&e))?;
            }
        }
        Ok(())
    };
    top_up()?;
    // SAFETY: plain COM call on a live interface.
    unsafe { client.Start() }.map_err(|e| error_text(&e))?;
    if let Some(started) = started.take() {
        let _ = started.send(Ok(()));
    }
    let mut result = Ok(());
    while !stop.load(Ordering::SeqCst) && result.is_ok() {
        std::thread::sleep(TOP_UP);
        result = top_up();
    }
    // SAFETY: plain COM call on a live interface.
    let _ = unsafe { client.Stop() };
    result
}
