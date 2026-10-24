//! Zero copy (docs/design.md §2.8): the P010 / NV12 textures on the capture device that the conversion shader
//! writes and NVENC reads, and the fence that tells the encoder's thread when the shader is done with one.
//!
//! The engine's thread converts into a surface and signals the fence behind it on the immediate context; the
//! writer thread waits for that value before NVENC maps the surface. The other direction needs no fence: the
//! engine converts only into a surface whose frame of the pool nobody holds (queued, the latest frame, or kept by
//! the encoder), and the encoder lets go of a frame only after its picture came out of NVENC.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread,
    ID3D11Texture2D, D3D11_BIND_UNORDERED_ACCESS, D3D11_FENCE_FLAG_NONE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::util::error_text;
use crate::args::CaptureFormat;
use crate::platform::FrameLayout;

/// The encoder's input surfaces, shared by the converter (engine thread) and the encoder (writer thread).
pub(crate) struct SurfaceRing {
    textures: Vec<ID3D11Texture2D>,
    layout: FrameLayout,
    fence: ID3D11Fence,
    /// The last value the engine's thread signalled (only that thread signals).
    signalled: AtomicU64,
    /// Per surface: the fence value at which the last conversion into it is finished on the GPU.
    written: Vec<AtomicU64>,
}

impl SurfaceRing {
    /// `count` textures of `layout` on `device`, which from now on is used by two threads (the converter on the
    /// engine's, NVENC on the writer's): Direct3D's own lock is switched on for it.
    pub(crate) fn new(
        device: &ID3D11Device,
        layout: FrameLayout,
        count: usize,
    ) -> Result<Self, String> {
        let format = match layout.format {
            CaptureFormat::P010 => DXGI_FORMAT_P010,
            CaptureFormat::Nv12 => DXGI_FORMAT_NV12,
            CaptureFormat::Auto => return Err("format must be resolved".to_owned()),
        };
        let device5 = device
            .cast::<ID3D11Device5>()
            .map_err(|_| "the GPU driver has no Direct3D 11.4 fences".to_owned())?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: plain object creation on a live device; the out pointer is a valid local.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }
            .map_err(|e| format!("cannot create a Direct3D 11 fence: {}", error_text(&e)))?;
        let fence = fence.ok_or("the fence was not created")?;

        // Written by the shader through one typed view per plane, read by NVENC (it needs no bind flag of its own).
        let desc = D3D11_TEXTURE2D_DESC {
            Width: layout.width,
            Height: layout.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut textures = Vec::with_capacity(count);
        for _ in 0..count {
            let mut texture = None;
            // SAFETY: as above; `desc` is a valid local.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.map_err(|e| {
                format!(
                    "cannot create a {} texture the shader writes: {}",
                    layout.format.name(),
                    error_text(&e)
                )
            })?;
            textures.push(texture.ok_or("the texture was not created")?);
        }

        // SAFETY: plain COM calls on the live device.
        unsafe {
            let multithread = device
                .cast::<ID3D11Multithread>()
                .map_err(|e| error_text(&e))?;
            let _ = multithread.SetMultithreadProtected(true);
        }
        Ok(Self {
            textures,
            layout,
            fence,
            signalled: AtomicU64::new(0),
            written: (0..count).map(|_| AtomicU64::new(0)).collect(),
        })
    }

    pub(crate) fn layout(&self) -> FrameLayout {
        self.layout
    }

    pub(crate) fn textures(&self) -> &[ID3D11Texture2D] {
        &self.textures
    }

    /// On the engine's thread, right after the commands that wrote `surface`: the fence is signalled behind them,
    /// and they go to the GPU now (the encoder's thread waits for them and cannot flush this context itself).
    pub(crate) fn written(
        &self,
        context: &ID3D11DeviceContext4,
        surface: usize,
    ) -> Result<(), String> {
        let value = self.signalled.load(Ordering::Relaxed) + 1;
        // SAFETY: the fence belongs to the context's device; plain calls on the engine's thread, which owns the
        // immediate context.
        unsafe {
            context
                .Signal(&self.fence, value)
                .map_err(|e| error_text(&e))?;
            context.Flush();
        }
        self.signalled.store(value, Ordering::Relaxed);
        self.written[surface].store(value, Ordering::Release);
        Ok(())
    }

    /// On the encoder's thread: waits up to `timeout` until the GPU has finished the last conversion into
    /// `surface`. `event` is an auto-reset event of the caller's.
    pub(crate) fn wait_written(
        &self,
        surface: usize,
        event: HANDLE,
        timeout: Duration,
    ) -> Result<(), String> {
        let value = self.written[surface].load(Ordering::Acquire);
        // SAFETY: fences may be waited on from any thread; the event is a live handle of the caller's.
        unsafe {
            if self.fence.GetCompletedValue() >= value {
                return Ok(());
            }
            // set at once when the value was reached in between
            self.fence
                .SetEventOnCompletion(value, event)
                .map_err(|e| error_text(&e))?;
            if WaitForSingleObject(event, timeout.as_millis() as u32) == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(format!(
                    "the GPU did not finish converting the picture within {} ms",
                    timeout.as_millis()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{mpsc, Arc};

    use windows::Win32::Graphics::Direct3D11::D3D11_SUBRESOURCE_DATA;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    };

    use super::*;
    use crate::args::{EncodeArgs, EngineEncoder};
    use crate::color;
    use crate::encode::{EncodeControl, EncodeStage};
    use crate::frame::{FramePool, SharedFrame};
    use crate::shader::{self, ShaderParams};
    use crate::win::converter::{create_shader_texture, GpuConverter};
    use crate::win::nvenc::{self, NvencEncoder};
    use crate::win::selftest::float_to_half;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 360;
    const FRAMES: usize = 240;

    /// ffmpeg in `HDRCAST_TOOLS`, then on PATH.
    fn ffmpeg() -> Option<PathBuf> {
        let tools = std::env::var_os("HDRCAST_TOOLS").map(PathBuf::from);
        let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>());
        tools
            .into_iter()
            .chain(path.into_iter().flatten())
            .map(|dir| dir.join("ffmpeg.exe"))
            .find(|exe| exe.is_file())
    }

    /// A source texture of one grey: an sRGB level (0..255) for NV12, an scRGB value for P010.
    fn grey(device: &ID3D11Device, format: CaptureFormat, value: f32) -> ID3D11Texture2D {
        let pixels = (WIDTH * HEIGHT) as usize;
        let (bytes, texel, dxgi) = if format == CaptureFormat::P010 {
            let [h0, h1] = float_to_half(value).to_le_bytes();
            let [o0, o1] = float_to_half(1.0).to_le_bytes();
            (
                [h0, h1, h0, h1, h0, h1, o0, o1].repeat(pixels),
                8,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
            )
        } else {
            let v = value as u8;
            ([v, v, v, 255].repeat(pixels), 4, DXGI_FORMAT_B8G8R8A8_UNORM)
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr().cast(),
            SysMemPitch: WIDTH * texel,
            SysMemSlicePitch: 0,
        };
        // `bytes` outlives the call that reads `initial`.
        create_shader_texture(device, WIDTH, HEIGHT, dxgi, Some(&initial))
            .expect("source texture")
            .0
    }

    /// The engine's two threads: this one converts a source that changes with every picture into the pool's
    /// surfaces, as fast as the pool lets it; the other one encodes them. Decoded again, every picture has to
    /// be the one the shader wrote for it: NVENC reading a surface too early, too late, or while the next
    /// picture is written into it (given back before a B-frame was done with it) would show a neighbour's grey.
    fn run(device: &ID3D11Device, ffmpeg: &Path, format: CaptureFormat, b_frames: u32) {
        let layout = FrameLayout {
            format,
            width: WIDTH,
            height: HEIGHT,
        };
        let hdr = format == CaptureFormat::P010;
        // the source's greys, and what the decoder has to give back for them (Y10 / Y8)
        let values: [f32; 3] = if hdr {
            [0.2, 2.0, 0.8]
        } else {
            [40.0, 200.0, 120.0]
        };
        let expected: Vec<i32> = values
            .iter()
            .map(|&v| {
                let v = f64::from(v);
                if hdr {
                    color::scrgb_grey_to_y10(v, 80.0)
                } else {
                    color::srgb_to_yuv8(v / 255.0, v / 255.0, v / 255.0).0
                }
            })
            .collect();
        let sources: Vec<ID3D11Texture2D> =
            values.iter().map(|&v| grey(device, format, v)).collect();

        let args = EncodeArgs {
            b_frames,
            bitrate_kbps: 8000,
            vbv_kbits: 2000,
            ..EncodeArgs::defaults(EngineEncoder::Nvenc, 60)
        };
        let mut encoder =
            NvencEncoder::open(&args, layout, 60, Some(device)).expect("NVENC on this device");
        let ring = Arc::new(
            SurfaceRing::new(device, layout, 6 + encoder.frames_held()).expect("surfaces"),
        );
        encoder
            .register(Arc::clone(&ring))
            .expect("NVENC takes the surfaces");
        let pool = FramePool::new(ring.textures().len(), 0);
        let mut converter = GpuConverter::new(device, format, WIDTH, HEIGHT).expect("converter");
        converter
            .use_surfaces(ring)
            .expect("the shader writes the surfaces");
        let params = ShaderParams {
            src_width: WIDTH,
            src_height: HEIGHT,
            desk_width: WIDTH,
            desk_height: HEIGHT,
            dst_width: WIDTH,
            dst_height: HEIGHT,
            flags: shader::FLAG_EXACT | if hdr { shader::FLAG_SOURCE_LINEAR } else { 0 },
            sdr_white: 80.0,
            cursor_w: 1,
            cursor_h: 1,
            ..ShaderParams::default()
        };

        // the sink's queue
        let (queue, frames) = mpsc::sync_channel::<SharedFrame>(4);
        let writer = std::thread::spawn(move || {
            let mut stage = EncodeStage::new(
                Box::new(encoder),
                60,
                30,
                EncodeControl::new(),
                Box::new(|_| {}),
            );
            let mut stream = Vec::new();
            for frame in frames {
                stage.push(&frame).expect("encode");
                stream.extend_from_slice(stage.output());
                stage.written();
            }
            stage.finish().expect("finish");
            stream.extend_from_slice(stage.output());
            stream
        });
        for i in 0..FRAMES {
            let frame = loop {
                match pool.try_rent() {
                    Some(frame) => break frame,
                    None => std::thread::yield_now(),
                }
            };
            converter.update_source(&sources[i % 3]).expect("source");
            converter
                .convert_into(&params, frame.index())
                .expect("convert");
            queue.send(Arc::new(frame)).expect("the writer runs");
        }
        drop(queue);
        let stream = writer.join().expect("the writer finished");

        let file = std::env::temp_dir().join(format!(
            "hdrcast-zero-copy-{}-{format}.ts",
            std::process::id()
        ));
        std::fs::write(&file, &stream).expect("write the stream");
        // as decoded (yuv420p / yuv420p10le, limited range): `gray` would be converted to full range
        let decoded = Command::new(ffmpeg)
            .args(["-v", "error", "-i"])
            .arg(&file)
            .args(["-f", "rawvideo", "-"])
            .output()
            .expect("ffmpeg runs");
        let _ = std::fs::remove_file(&file);
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let samples: Vec<i32> = if hdr {
            decoded
                .stdout
                .chunks_exact(2)
                .map(|s| i32::from(u16::from_le_bytes([s[0], s[1]])))
                .collect()
        } else {
            decoded.stdout.iter().map(|&s| i32::from(s)).collect()
        };
        let (luma, per_picture) = ((WIDTH * HEIGHT) as usize, (WIDTH * HEIGHT * 3 / 2) as usize);
        assert_eq!(
            samples.len(),
            FRAMES * per_picture,
            "{format}, {b_frames} B-frames: every picture came out"
        );
        for (i, picture) in samples.chunks_exact(per_picture).enumerate() {
            let picture = &picture[..luma];
            let (low, high) = (picture.iter().min().unwrap(), picture.iter().max().unwrap());
            let want = expected[i % 3];
            assert!(
                (low - want).abs() <= 3 && (high - want).abs() <= 3,
                "{format}, {b_frames} B-frames, picture {i}: luma {low}..{high}, the shader wrote {want}"
            );
        }
    }

    /// The engine's first frame, before the screen delivered one, is a cleared surface: limited-range black as in
    /// memory.
    #[test]
    fn a_cleared_surface_is_black() {
        let Ok((device, _)) = nvenc::nvidia_device() else {
            eprintln!("note: no NVIDIA GPU here; skipping the zero-copy check");
            return;
        };
        for format in [CaptureFormat::P010, CaptureFormat::Nv12] {
            let layout = FrameLayout {
                format,
                width: 64,
                height: 36,
            };
            let mut converter = GpuConverter::new(&device, format, 64, 36).expect("converter");
            let ring = SurfaceRing::new(&device, layout, 2).expect("surfaces");
            converter
                .use_surfaces(Arc::new(ring))
                .expect("the shader writes the surfaces");
            converter.clear_surface(1).expect("clear");
            let mut black = vec![0u8; layout.frame_bytes()];
            color::fill_black(&mut black, format, 64, 36);
            assert!(
                converter.read_surface(1).expect("read back") == black,
                "{format}"
            );
        }
    }

    #[test]
    fn every_encoded_picture_is_the_one_the_shader_wrote() {
        let Some(ffmpeg) = ffmpeg() else {
            eprintln!("note: ffmpeg not found (set HDRCAST_TOOLS or put it on PATH); skipping the zero-copy check");
            return;
        };
        let Ok((device, _)) = nvenc::nvidia_device() else {
            eprintln!("note: no NVIDIA GPU here; skipping the zero-copy check");
            return;
        };
        run(&device, &ffmpeg, CaptureFormat::Nv12, 0);
        run(&device, &ffmpeg, CaptureFormat::P010, 2);
    }
}
