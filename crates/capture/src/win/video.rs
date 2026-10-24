//! The Windows [`VideoSource`]: Desktop Duplication feeding the conversion shader.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::converter::GpuConverter;
use super::duplicator::{AcquireResult, DesktopDuplicator};
use super::nvenc::{self, NvencEncoder};
use super::surfaces::SurfaceRing;
use super::util::DpiAwareThread;
use crate::args::{CaptureArgs, CaptureFormat, EncodeArgs, EngineEncoder};
use crate::encode::VideoEncoder;
use crate::log;
use crate::platform::{FrameLayout, FrameStatus, VideoError, VideoSource};
use crate::shader::{self, ShaderParams};

/// Switches zero copy off (design.md section 2.8): the frames go to the encoder through memory as before, for a
/// comparison or a driver that gets the shared device wrong.
const NO_ZERO_COPY_ENV: &str = "HDRCAST_NO_ZERO_COPY";

pub(crate) struct DxgiVideoSource {
    // Field order is drop order: the converter's views and buffers go before the device owner.
    converter: GpuConverter,
    duplicator: DesktopDuplicator,
    layout: FrameLayout,
    cursor: bool,
    sdr_white: f32,
    /// Keeps the engine thread per-monitor DPI aware while the duplication exists (see [`DpiAwareThread`]).
    _dpi: DpiAwareThread,
}

fn even(v: u32) -> u32 {
    if v < 2 {
        2
    } else {
        v & !1
    }
}

/// Opens the output and builds the converter; the error text follows `capture: ` in the log.
pub(crate) fn open(args: &CaptureArgs) -> Result<DxgiVideoSource, String> {
    let dpi = DpiAwareThread::enter();
    let output_index =
        u32::try_from(args.output_index).map_err(|_| "--output <idx> is required".to_owned())?;
    let duplicator = DesktopDuplicator::open(output_index, args.format != CaptureFormat::Nv12)?;

    let format = match args.format {
        CaptureFormat::P010 => CaptureFormat::P010,
        CaptureFormat::Nv12 => CaptureFormat::Nv12,
        CaptureFormat::Auto if duplicator.info.hdr && duplicator.source_is_float => {
            CaptureFormat::P010
        }
        CaptureFormat::Auto => CaptureFormat::Nv12,
    };
    let (src_w, src_h) = (duplicator.desktop_width, duplicator.desktop_height);
    let (dst_w, dst_h) = if args.has_explicit_size() {
        (args.width, args.height)
    } else {
        (even(src_w), even(src_h))
    };

    log::info(format!(
        "capture: output {} ({} {}) {}x{} rot{} hdr={} source={} -> {} {}x{} @{}fps",
        args.output_index,
        duplicator.info.device_name,
        duplicator.info.friendly_name,
        src_w,
        src_h,
        duplicator.rotation,
        if duplicator.info.hdr { "True" } else { "False" },
        if duplicator.source_is_float {
            "rgbaf16(scRGB)"
        } else {
            "bgra8(sRGB)"
        },
        format.name(),
        dst_w,
        dst_h,
        args.fps
    ));

    if format == CaptureFormat::P010 && !duplicator.source_is_float {
        log::warn("capture: the desktop is not delivering FP16; HDR highlights above SDR white are not available");
    }

    let converter = GpuConverter::new(duplicator.device(), format, dst_w, dst_h)
        .map_err(|e| format!("GPU converter failed: {e}"))?;

    Ok(DxgiVideoSource {
        converter,
        duplicator,
        layout: FrameLayout {
            format,
            width: dst_w,
            height: dst_h,
        },
        cursor: args.cursor,
        sdr_white: args.sdr_white,
        _dpi: dpi,
    })
}

impl DxgiVideoSource {
    fn build_params(&self) -> ShaderParams {
        let d = &self.duplicator;
        let mut flags = 0;
        if d.source_is_float {
            flags |= shader::FLAG_SOURCE_LINEAR;
        }
        if self.cursor && d.cursor_visible && d.cursor_width > 0 {
            flags |= shader::FLAG_CURSOR_VISIBLE;
        }
        let exact = d.rotation == 0
            && self.layout.width == d.desktop_width
            && self.layout.height == d.desktop_height;
        if exact {
            flags |= shader::FLAG_EXACT;
        }
        if self.layout.format == CaptureFormat::Nv12 && d.source_is_float {
            flags |= shader::FLAG_SOURCE_IS_HDR_FORCED_SDR;
        }

        ShaderParams {
            src_width: d.source_width,
            src_height: d.source_height,
            desk_width: d.desktop_width,
            desk_height: d.desktop_height,
            dst_width: self.layout.width,
            dst_height: self.layout.height,
            rotation: d.rotation,
            flags,
            sdr_white: self.sdr_white,
            cursor_x: d.cursor_x,
            cursor_y: d.cursor_y,
            cursor_w: d.cursor_width.max(1),
            cursor_h: d.cursor_height.max(1),
            ..ShaderParams::default()
        }
    }

    /// NVENC on the capture device, reading the surfaces the shader writes. Possible when the screen is on the
    /// NVIDIA GPU (on a laptop it is often the integrated one, whose pictures have to cross into the NVIDIA GPU's
    /// memory anyway: that is the path through memory).
    fn open_surface_encoder(
        &mut self,
        args: &EncodeArgs,
        fps: u32,
        frames: usize,
    ) -> Result<NvencEncoder, String> {
        match args.encoder {
            EngineEncoder::Nvenc => {}
        }
        if std::env::var_os(NO_ZERO_COPY_ENV).is_some() {
            return Err(format!("{NO_ZERO_COPY_ENV} is set"));
        }
        let device = self.duplicator.device().clone();
        if !nvenc::is_nvidia_device(&device) {
            return Err(format!("the screen is on {}", self.duplicator.info.adapter));
        }
        // The session first: it says whether NVENC takes this size and format at all.
        let mut encoder = NvencEncoder::open(args, self.layout, fps, Some(&device))?;
        // What the engine holds plus the pictures a run of B-frames keeps in the encoder.
        let count = frames + encoder.frames_held();
        let ring = Arc::new(SurfaceRing::new(&device, self.layout, count)?);
        encoder.register(Arc::clone(&ring))?;
        self.converter.use_surfaces(ring)?;
        Ok(encoder)
    }

    /// Waits for the screen and converts the newest picture with `convert`, as [`VideoSource::next_frame`] says.
    fn next(
        &mut self,
        timeout: Duration,
        convert: impl FnOnce(&mut GpuConverter, &ShaderParams) -> Result<(), String>,
    ) -> Result<FrameStatus, VideoError> {
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let acquired = self
            .duplicator
            .try_acquire(timeout_ms, &mut self.converter)
            .map_err(|message| VideoError::Lost(Some(message)))?;
        if acquired == AcquireResult::AccessLost {
            return Err(VideoError::Lost(None));
        }

        if let Some(cursor) = self.duplicator.take_pending_cursor() {
            if let Err(e) = self
                .converter
                .set_cursor(&cursor.bgra, cursor.width, cursor.height)
            {
                log::warn(format!("capture: cursor upload failed: {e}"));
            }
        }
        let pointer_moved = acquired == AcquireResult::PointerOnly && self.cursor;
        if (acquired == AcquireResult::NewFrame || pointer_moved) && self.converter.has_source() {
            let started = Instant::now();
            let params = self.build_params();
            convert(&mut self.converter, &params).map_err(VideoError::Fatal)?;
            return Ok(FrameStatus::New {
                gpu_ms: started.elapsed().as_secs_f64() * 1000.0,
            });
        }
        Ok(FrameStatus::Unchanged)
    }
}

impl VideoSource for DxgiVideoSource {
    fn layout(&self) -> FrameLayout {
        self.layout
    }

    fn next_frame(&mut self, dst: &mut [u8], timeout: Duration) -> Result<FrameStatus, VideoError> {
        self.next(timeout, |converter, params| converter.convert(params, dst))
    }

    fn recover(&mut self) -> Result<(), String> {
        self.duplicator.recreate()
    }

    fn surface_encoder(
        &mut self,
        args: &EncodeArgs,
        fps: u32,
        frames: usize,
    ) -> Option<Box<dyn VideoEncoder>> {
        match self.open_surface_encoder(args, fps, frames) {
            Ok(encoder) => Some(Box::new(encoder)),
            Err(reason) => {
                log::info(format!(
                    "capture: no zero copy ({reason}): the frames go to the encoder through memory"
                ));
                None
            }
        }
    }

    fn next_surface(
        &mut self,
        surface: usize,
        timeout: Duration,
    ) -> Result<FrameStatus, VideoError> {
        self.next(timeout, |converter, params| {
            converter.convert_into(params, surface)
        })
    }

    fn black_surface(&mut self, surface: usize) -> Result<(), String> {
        self.converter.clear_surface(surface)
    }
}
