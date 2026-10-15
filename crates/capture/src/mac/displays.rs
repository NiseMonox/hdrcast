//! The displays in the order `capture --output` indexes them: the main display first, then by ascending
//! `CGDirectDisplayID`.
//!
//! Geometry and refresh rate come from Quartz Display Services (thread-safe, and needing no permission).
//! The name and the HDR state exist only on `NSScreen`, which AppKit binds to the main thread: when the
//! caller is not on it (the app listing its displays from a worker thread), the question is sent to the
//! main dispatch queue with a time limit - the app's main thread runs the queue; the engine process of the
//! command line asks from its main thread directly. Should nobody serve the main queue, the displays are
//! still listed, without names and as SDR.

use std::panic::catch_unwind;
use std::sync::mpsc;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::autoreleasepool;
use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode,
    CGDisplayPixelsHigh, CGDisplayPixelsWide, CGDisplayRotation, CGError, CGGetActiveDisplayList,
    CGMainDisplayID,
};
use objc2_foundation::{NSNumber, NSString};

use crate::log;
use crate::output::OutputInfo;

/// More displays than a Mac drives; `--output` accepts 0..=63.
const MAX_DISPLAYS: usize = 64;

/// How long a caller that is not on the main thread waits for the main thread to answer.
const MAIN_THREAD_WAIT: Duration = Duration::from_millis(1500);

/// One active display.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Display {
    pub id: CGDirectDisplayID,
    /// Size of the desktop on this display in pixels (not points), as it is shown - i.e. after rotation.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Origin in the global display coordinate space (points, y down).
    pub x: i32,
    pub y: i32,
    /// 0, 90, 180 or 270.
    pub rotation_degrees: u32,
    /// Hz; 0 = unknown.
    pub refresh_rate: f64,
    /// `NSScreen.localizedName`; empty when AppKit could not be asked.
    pub name: String,
    /// `NSScreen.maximumPotentialExtendedDynamicRangeColorComponentValue`: above 1.0 the display can show
    /// brighter-than-SDR-white content right now (an XDR panel, or an external display with HDR switched on
    /// in System Settings). 1.0 = SDR, also when AppKit could not be asked.
    pub edr_headroom: f64,
}

impl Display {
    /// The display is in a state that shows HDR content.
    pub(super) fn edr(&self) -> bool {
        self.edr_headroom > 1.0
    }

    /// The description `list-outputs` prints. `hdr` says whether the engine captures this display as HDR10
    /// (the app picks p010 or nv12 for ffmpeg from it), so it is the caller's verdict, not just the panel's.
    pub(super) fn to_output_info(&self, index: u32, hdr: bool) -> OutputInfo {
        OutputInfo {
            index,
            adapter_index: 0,
            output_index_on_adapter: index,
            adapter: String::new(),
            // `hdrcast-control` parses this back into the CGDirectDisplayID.
            device_name: self.id.to_string(),
            friendly_name: self.name.clone(),
            width: self.pixel_width as i32,
            height: self.pixel_height as i32,
            x: self.x,
            y: self.y,
            // DXGI_MODE_ROTATION numbering, which the field uses on every platform.
            rotation: match self.rotation_degrees {
                90 => 2,
                180 => 3,
                270 => 4,
                _ => 1,
            },
            refresh_rate: self.refresh_rate,
            attached_to_desktop: true,
            color_space: if hdr {
                OutputInfo::COLOR_SPACE_HDR10
            } else {
                -1
            },
            hdr,
            bits_per_color: 0,
            // No documented way to read luminance in nit: EDR headroom is relative to SDR white.
            min_luminance: 0.0,
            max_luminance: 0.0,
            max_full_frame_luminance: 0.0,
        }
    }
}

/// The active displays, main display first.
pub(super) fn list() -> Result<Vec<Display>, String> {
    let mut ids = active_display_ids()?;
    let main = CGMainDisplayID();
    sort_main_first(&mut ids, main);
    let screens = screen_facts();
    Ok(ids
        .into_iter()
        .map(|id| describe(id, screens.iter().find(|screen| screen.id == id)))
        .collect())
}

/// The main display first, the others by ascending id: an order that survives re-plugging.
fn sort_main_first(ids: &mut [CGDirectDisplayID], main: CGDirectDisplayID) {
    ids.sort_unstable_by_key(|id| (*id != main, *id));
}

fn active_display_ids() -> Result<Vec<CGDirectDisplayID>, String> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: `ids` has room for MAX_DISPLAYS entries, which is the limit passed; `count` is a valid
    // out-pointer.
    let status =
        unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if status != CGError::Success {
        return Err(format!(
            "cannot list the displays (CGGetActiveDisplayList: error {}); is a user logged in at this Mac's screen?",
            status.0
        ));
    }
    Ok(ids[..(count as usize).min(MAX_DISPLAYS)].to_vec())
}

fn describe(id: CGDirectDisplayID, screen: Option<&ScreenFacts>) -> Display {
    let bounds = CGDisplayBounds(id);
    let mode = CGDisplayCopyDisplayMode(id);
    let mode = mode.as_deref();

    // Pixels per point of the current mode. The bounds are those of the desktop as shown (rotated), the
    // mode may describe the panel (unrotated): scaling the bounds gives the shown size either way.
    let points_wide = CGDisplayMode::width(mode);
    let pixels_wide = CGDisplayMode::pixel_width(mode);
    let (pixel_width, pixel_height) = if mode.is_some() && points_wide > 0 && pixels_wide > 0 {
        let scale = pixels_wide as f64 / points_wide as f64;
        (
            (bounds.size.width * scale).round() as u32,
            (bounds.size.height * scale).round() as u32,
        )
    } else {
        (
            CGDisplayPixelsWide(id) as u32,
            CGDisplayPixelsHigh(id) as u32,
        )
    };

    // Built-in panels report 0 here ("no fixed rate"); the screen knows what it can do.
    let mut refresh_rate = CGDisplayMode::refresh_rate(mode);
    if !refresh_rate.is_finite() || refresh_rate < 1.0 {
        refresh_rate = screen.map_or(0.0, |screen| screen.max_fps.max(0) as f64);
    }

    let rotation = CGDisplayRotation(id);
    Display {
        id,
        pixel_width,
        pixel_height,
        x: bounds.origin.x.round() as i32,
        y: bounds.origin.y.round() as i32,
        rotation_degrees: if rotation.is_finite() {
            (rotation.round() as i64).rem_euclid(360) as u32
        } else {
            0
        },
        refresh_rate,
        name: screen.map(|screen| screen.name.clone()).unwrap_or_default(),
        edr_headroom: screen.map_or(1.0, |screen| screen.edr_headroom),
    }
}

/// What only AppKit knows about a display. Plain data, so it can leave the main thread.
#[derive(Debug, Clone)]
struct ScreenFacts {
    id: CGDirectDisplayID,
    name: String,
    edr_headroom: f64,
    max_fps: isize,
}

fn read_screens(mtm: MainThreadMarker) -> Vec<ScreenFacts> {
    autoreleasepool(|_| {
        // The CGDirectDisplayID of a screen is the "NSScreenNumber" entry of its device description.
        let screen_number = NSString::from_str("NSScreenNumber");
        NSScreen::screens(mtm)
            .to_vec()
            .iter()
            .filter_map(|screen| {
                let number = screen.deviceDescription().objectForKey(&screen_number)?;
                let id = number.downcast_ref::<NSNumber>()?.unsignedIntValue();
                Some(ScreenFacts {
                    id,
                    name: screen.localizedName().to_string(),
                    edr_headroom: screen.maximumPotentialExtendedDynamicRangeColorComponentValue(),
                    max_fps: screen.maximumFramesPerSecond(),
                })
            })
            .collect()
    })
}

fn screen_facts() -> Vec<ScreenFacts> {
    if let Some(mtm) = MainThreadMarker::new() {
        return read_screens(mtm);
    }
    let (answer, waiting) = mpsc::channel();
    // Asynchronous with a bounded wait, never dispatch_sync: a main thread that does not run its queue
    // (a command-line process whose main thread is busy running the engine, or one that is itself waiting
    // for this call) would block the caller forever. A block that runs after the wait has ended finds the
    // receiver gone and its answer is dropped.
    DispatchQueue::main().exec_async(move || {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // A panic must not unwind into libdispatch; the caller then simply gets no answer.
        if let Ok(screens) = catch_unwind(|| read_screens(mtm)) {
            let _ = answer.send(screens);
        }
    });
    match waiting.recv_timeout(MAIN_THREAD_WAIT) {
        Ok(screens) => screens,
        Err(_) => {
            log::warn(
                "displays: the main thread does not answer, so display names and HDR states are \
                 unknown (NSScreen can only be asked there); every display is treated as SDR",
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_main_display_comes_first_then_ascending_ids() {
        let mut ids = [7, 3, 69_733_382, 1, 4];
        sort_main_first(&mut ids, 4);
        assert_eq!(ids, [4, 1, 3, 7, 69_733_382]);
        // a main display that is not in the list (cannot happen) leaves a plain ascending order
        sort_main_first(&mut ids, 99);
        assert_eq!(ids, [1, 3, 4, 7, 69_733_382]);
    }

    #[test]
    fn a_display_is_described_the_way_the_other_crates_expect() {
        let display = Display {
            id: 69_733_382,
            pixel_width: 3456,
            pixel_height: 2234,
            x: -1728,
            y: 0,
            rotation_degrees: 270,
            refresh_rate: 120.0,
            name: "Built-in Retina Display".to_owned(),
            edr_headroom: 16.0,
        };
        assert!(display.edr());
        let info = display.to_output_info(1, true);
        // hdrcast-control parses the device name back into the CGDirectDisplayID
        assert_eq!(info.device_name.parse::<u32>(), Ok(69_733_382));
        assert_eq!((info.width, info.height), (3456, 2234));
        assert_eq!(info.rotation_name(), "270");
        assert!(info.hdr);
        assert_eq!(
            info.label(),
            "1: Built-in Retina Display (69733382) 3456x2234@120 HDR"
        );
        let sdr = display.to_output_info(1, false);
        assert!(!sdr.hdr);
        assert_eq!(sdr.color_space, -1);
    }
}
