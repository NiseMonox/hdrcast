//! Playing sound, for a viewer in control mode (docs/design.md §11.4): 48 kHz stereo 16-bit on the default output,
//! pulled from the caller as the sound card needs it. The viewer keeps its own small buffer in front of it (the
//! network's jitter) and the output keeps as little as it can on its side, so the sound stays with the picture.
//! Windows: WASAPI in shared mode, about 20 ms queued at the device, reopened on the new default when the default
//! output changes. macOS: an AudioQueue (not yet run on a Mac). Elsewhere: none.

/// Fills the buffer it is given (interleaved stereo, 16-bit) with what plays next; silence when there is nothing.
pub type PullFn = Box<dyn FnMut(&mut [i16]) + Send>;

/// Plays until dropped.
pub struct SoundOutput {
    _inner: Inner,
}

#[cfg(windows)]
type Inner = crate::win::playback::Output;
#[cfg(target_os = "macos")]
type Inner = crate::mac::playback::Output;
#[cfg(not(any(windows, target_os = "macos")))]
type Inner = ();

impl SoundOutput {
    /// Starts playing on the default output. The error says why there is no sound (the picture goes on without).
    #[allow(unreachable_code)]
    pub fn start(pull: PullFn) -> Result<SoundOutput, String> {
        #[cfg(windows)]
        let inner = crate::win::playback::Output::start(pull)?;
        #[cfg(target_os = "macos")]
        let inner = crate::mac::playback::Output::start(pull)?;
        #[cfg(not(any(windows, target_os = "macos")))]
        let inner = {
            drop(pull);
            return Err("no sound output on this platform".to_owned());
        };
        Ok(SoundOutput { _inner: inner })
    }
}
