//! The macOS [`VideoSource`]: the screen output of the session's `SCStream`.
//!
//! ScreenCaptureKit pushes pictures; the engine pulls "the newest picture, if it changed". The callback
//! parks every complete picture in the session's slot (see `output`), `next_frame` waits on that slot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::displays::{self, Display};
use super::output::{FrameSpec, PixelPlan, Probe};
use super::stream::{Session, StreamParams};
use super::{permission, system};
use crate::args::{CaptureArgs, CaptureFormat};
use crate::log;
use crate::platform::{FrameLayout, FrameStatus, VideoError, VideoSource};

/// How long the first picture of an HDR stream is waited for, to see that it has the configured format
/// before the engine is told the layout. A stream delivers one right after it started, also on an idle
/// screen.
const HDR_PROBE: Duration = Duration::from_millis(1500);

fn even(value: u32) -> u32 {
    if value < 2 {
        2
    } else {
        value & !1
    }
}

/// Whether the engine captures `display` as HDR10 when left to choose (`--format auto`), and otherwise why
/// not. `list-outputs` reports the same verdict as `hdr`, because the app decides from it which pixel
/// format it tells ffmpeg to expect.
pub(super) fn hdr_verdict(display: &Display) -> Result<(), &'static str> {
    if let Some(obstacle) = system::hdr_capture_obstacle() {
        return Err(obstacle);
    }
    if !display.edr() {
        return Err("the display is not in an HDR (extended dynamic range) state");
    }
    Ok(())
}

/// The ways to ask for the pictures, in the order they are tried.
fn plans(format: CaptureFormat, display: &Display) -> Vec<PixelPlan> {
    if format == CaptureFormat::Nv12 {
        return vec![PixelPlan::Nv12];
    }
    match hdr_verdict(display) {
        // The plain 10-bit 4:2:0 format first (no conversion at all); the documented 4:4:4 one next;
        // SDR when this system delivers neither.
        Ok(()) => vec![PixelPlan::P010, PixelPlan::P010From444, PixelPlan::Nv12],
        Err(reason) => {
            if format == CaptureFormat::P010 {
                log::warn(format!(
                    "capture: --format p010 was asked for, but {reason}; capturing SDR instead - what \
                     leaves the engine is nv12 and its consumer has to read it as that"
                ));
            } else if display.edr() {
                log::info(format!(
                    "capture: the display could show HDR, but {reason}; capturing SDR (nv12)"
                ));
            }
            vec![PixelPlan::Nv12]
        }
    }
}

/// Opens the display `args.output_index` and starts capturing it; the error text follows `capture: ` in
/// the log.
pub(super) fn open(args: &CaptureArgs) -> Result<ScreenVideoSource, String> {
    if !system::screen_capture_kit_available() {
        return Err(
            "ScreenCaptureKit is not available: screen capture needs macOS 12.3 or later"
                .to_owned(),
        );
    }
    let index =
        usize::try_from(args.output_index).map_err(|_| "--output <idx> is required".to_owned())?;
    let all = displays::list()?;
    let display = all.get(index).ok_or_else(|| {
        format!(
            "output {index} does not exist: {} display(s) are active (see list-outputs)",
            all.len()
        )
    })?;
    permission::ensure_screen_capture_access()?;

    let (width, height) = if args.has_explicit_size() {
        (args.width, args.height)
    } else {
        (even(display.pixel_width), even(display.pixel_height))
    };
    let fps = args.fps.max(1);
    // The sound comes from the same stream, so it has to be switched on before the stream starts;
    // `open_audio` then only says where it goes. A dump has no audio (see the engine).
    let audio = args.audio && args.dump_path.is_none() && system::audio_capture_supported();

    let candidates = plans(args.format, display);
    log::info(format!(
        "capture: output {} (display {} {}) {}x{} rot{} hdr={} -> {} {}x{} @{}fps",
        args.output_index,
        display.id,
        display.name,
        display.pixel_width,
        display.pixel_height,
        display.rotation_degrees,
        if display.edr() { "True" } else { "False" },
        candidates[0].capture_format().name(),
        width,
        height,
        fps
    ));

    let mut failure = String::new();
    for (position, plan) in candidates.iter().copied().enumerate() {
        let next = candidates.get(position + 1).copied();
        let params = StreamParams {
            display_id: display.id,
            width,
            height,
            fps,
            cursor: args.cursor,
            plan,
            video: true,
            audio,
        };
        let refused = match Session::start(params) {
            // A format the header does not list has to be seen to arrive before the engine relies on it.
            Ok(session) if plan.is_hdr() => match session.shared().probe_first_frame(HDR_PROBE) {
                Probe::Frame => {
                    log::info(format!(
                        "capture: HDR pictures arrive as {}",
                        plan.describe()
                    ));
                    return Ok(ScreenVideoSource::new(session));
                }
                Probe::Nothing => {
                    log::info(format!(
                        "capture: no picture within {} ms to check the HDR format ({}) against; carrying on with it",
                        HDR_PROBE.as_millis(),
                        plan.describe()
                    ));
                    return Ok(ScreenVideoSource::new(session));
                }
                // dropping the session stops its stream
                Probe::Refused(message) => message,
            },
            Ok(session) => {
                if position > 0 {
                    log::warn(
                        "capture: HDR capture does not work on this system; capturing SDR instead - what \
                         leaves the engine is nv12 and its consumer has to read it as that",
                    );
                }
                return Ok(ScreenVideoSource::new(session));
            }
            Err(message) => message,
        };
        match next {
            Some(next) => log::warn(format!(
                "capture: {} does not work here ({refused}); trying {}",
                plan.describe(),
                next.describe()
            )),
            None => failure = refused,
        }
    }
    Err(failure)
}

pub(super) struct ScreenVideoSource {
    session: Arc<Session>,
    layout: FrameLayout,
    /// The number of the picture last handed to the engine (see `VideoSlot::seq`).
    seen: u64,
    /// The third of the three frame buffers: swapped with the slot's, then copied from without the lock.
    spare: Vec<u8>,
    /// The stream ended and has not been restarted yet.
    lost: bool,
}

impl ScreenVideoSource {
    fn new(session: Arc<Session>) -> Self {
        // A session opened by `open` always has pictures.
        let spec = session.shared().frame().unwrap_or(FrameSpec {
            plan: PixelPlan::Nv12,
            width: 2,
            height: 2,
        });
        Self {
            layout: FrameLayout {
                format: spec.plan.capture_format(),
                width: spec.width,
                height: spec.height,
            },
            seen: 0,
            spare: vec![0x10; spec.frame_bytes()],
            lost: false,
            session,
        }
    }

    pub(super) fn session(&self) -> &Arc<Session> {
        &self.session
    }
}

impl VideoSource for ScreenVideoSource {
    fn layout(&self) -> FrameLayout {
        self.layout
    }

    fn next_frame(&mut self, dst: &mut [u8], timeout: Duration) -> Result<FrameStatus, VideoError> {
        if self.lost {
            return Err(VideoError::Lost(None));
        }
        let shared = self.session.shared();
        let deadline = Instant::now() + timeout;
        let mut slot = shared.video();
        loop {
            if let Some(message) = &slot.fatal {
                return Err(VideoError::Fatal(message.clone()));
            }
            if let Some(stop) = slot.stopped.take() {
                if stop.by_user {
                    // Restarting would override what the user just did in the system's own menu.
                    return Err(VideoError::Fatal(format!(
                        "the capture was stopped from the macOS screen-sharing menu ({})",
                        stop.message
                    )));
                }
                self.lost = true;
                return Err(VideoError::Lost(Some(format!(
                    "ScreenCaptureKit ended the stream: {}",
                    stop.message
                ))));
            }
            if slot.seq != self.seen {
                break;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(FrameStatus::Unchanged);
            }
            // The lock is released while waiting.
            slot = shared.wait(slot, left);
        }
        self.seen = slot.seq;
        let copy_ms = slot.copy_ms;
        std::mem::swap(&mut slot.front, &mut self.spare);
        drop(slot);

        if self.spare.len() != dst.len() {
            return Err(VideoError::Fatal(format!(
                "the engine's frame buffer has {} bytes, a captured frame {}",
                dst.len(),
                self.spare.len()
            )));
        }
        let started = Instant::now();
        dst.copy_from_slice(&self.spare);
        Ok(FrameStatus::New {
            gpu_ms: copy_ms + started.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn recover(&mut self) -> Result<(), String> {
        self.session.restart()?;
        self.lost = false;
        Ok(())
    }
}

impl Drop for ScreenVideoSource {
    fn drop(&mut self) {
        self.session.detach_video();
    }
}
