//! GPU side of the capture engine: compiles the conversion compute shader, runs it over the duplicated
//! desktop texture and reads the finished P010 / NV12 frame back to system memory (design.md section 2.3),
//! or, with zero copy, writes it straight into the encoder's surfaces (section 2.8).

use std::ffi::{c_void, CString};
use std::sync::Arc;

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11ComputeShader, ID3D11Device, ID3D11DeviceContext, ID3D11DeviceContext4,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11UnorderedAccessView,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_UNORDERED_ACCESS,
    D3D11_BUFFER_DESC, D3D11_BUFFER_UAV, D3D11_BUFFER_UAV_FLAG_RAW, D3D11_COMPARISON_NEVER,
    D3D11_CPU_ACCESS_READ, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS, D3D11_SAMPLER_DESC,
    D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_UAV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_UAV_DIMENSION_BUFFER, D3D11_UAV_DIMENSION_TEXTURE2D, D3D11_UNORDERED_ACCESS_VIEW_DESC,
    D3D11_UNORDERED_ACCESS_VIEW_DESC_0, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16_UINT, DXGI_FORMAT_R16_UINT,
    DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R8G8_UINT, DXGI_FORMAT_R8_UINT, DXGI_SAMPLE_DESC,
};

use super::surfaces::SurfaceRing;
use super::util::error_text;
use crate::args::CaptureFormat;
use crate::color;
use crate::shader::{self, ShaderParams};

/// Compiles one entry point of the conversion shader with the profile and optimisation level the engine
/// uses. The source goes to the compiler as bytes with their exact length (see [`shader::SOURCE`]).
pub(crate) fn compile_shader(source: &[u8], entry: &str) -> Result<Vec<u8>, String> {
    let entry = CString::new(entry).map_err(|e| e.to_string())?;
    let profile = CString::new(shader::PROFILE).map_err(|e| e.to_string())?;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: `source` is valid for `source.len()` bytes; the name, entry point and profile are
    // NUL-terminated strings that outlive the call; the out pointers are valid.
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast::<c_void>(),
            source.len(),
            s!("hdrcast_convert.hlsl"),
            None,
            None,
            PCSTR(entry.as_ptr().cast()),
            PCSTR(profile.as_ptr().cast()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let message = errors
        .as_ref()
        .map(blob_bytes)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    let message = message.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    match (result, code) {
        (Ok(()), Some(code)) => Ok(blob_bytes(&code).to_vec()),
        (Err(e), _) if message.is_empty() => Err(error_text(&e)),
        _ => Err(message.to_owned()),
    }
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // SAFETY: a blob owns `GetBufferSize()` bytes at `GetBufferPointer()` for as long as it lives, and the
    // returned slice borrows the blob.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}

fn align16(v: usize) -> u32 {
    ((v + 15) & !15) as u32
}

struct SourceCopy {
    texture: ID3D11Texture2D,
    view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
}

struct CursorTexture {
    texture: ID3D11Texture2D,
    view: Option<ID3D11ShaderResourceView>,
    width: u32,
    height: u32,
}

/// Where the shader writes.
enum Output {
    /// The whole frame into a raw buffer, copied to a staging buffer and read back into memory.
    Memory {
        shader: ID3D11ComputeShader,
        buffer: ID3D11Buffer,
        view: Option<ID3D11UnorderedAccessView>,
        staging: ID3D11Buffer,
    },
    /// Zero copy: into the planes of the encoder's surfaces, through a luma and a chroma view of each.
    Surfaces {
        shader: ID3D11ComputeShader,
        ring: Arc<SurfaceRing>,
        views: Vec<[Option<ID3D11UnorderedAccessView>; 2]>,
        context: ID3D11DeviceContext4,
    },
}

pub(crate) struct GpuConverter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    format: CaptureFormat,
    dst_width: u32,
    dst_height: u32,
    frame_bytes: usize,

    output: Output,
    constants: Option<ID3D11Buffer>,
    sampler: Option<ID3D11SamplerState>,

    source: Option<SourceCopy>,
    cursor: Option<CursorTexture>,
}

impl GpuConverter {
    pub(crate) fn new(
        device: &ID3D11Device,
        format: CaptureFormat,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<Self, String> {
        let entry = match format {
            CaptureFormat::P010 => shader::ENTRY_P010,
            CaptureFormat::Nv12 => shader::ENTRY_NV12,
            CaptureFormat::Auto => return Err("format must be resolved".to_owned()),
        };
        let frame_bytes = color::frame_bytes(format, dst_width, dst_height);
        if frame_bytes > i32::MAX as usize {
            return Err("frame too large".to_owned());
        }
        let buffer_bytes = align16(frame_bytes);

        // bytes, not a string: see shader::SOURCE
        let bytecode = compile_shader(shader::source_bytes(), entry)
            .map_err(|e| format!("conversion shader failed to compile: {e}"))?;

        let text = |e: windows::core::Error| error_text(&e);
        // SAFETY (all blocks below): plain D3D11 object creation on a live device; every descriptor and out
        // pointer is a valid local for the duration of its call.
        let context = unsafe { device.GetImmediateContext() }.map_err(text)?;

        let mut shader = None;
        unsafe { device.CreateComputeShader(&bytecode, None, Some(&mut shader)) }.map_err(text)?;
        let shader = shader.ok_or("the compute shader was not created")?;

        // Raw (ByteAddress) UAV buffer holding the complete frame, plus a staging copy for CPU readback.
        let out_desc = D3D11_BUFFER_DESC {
            ByteWidth: buffer_bytes,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
            StructureByteStride: 0,
        };
        let mut out_buffer = None;
        unsafe { device.CreateBuffer(&out_desc, None, Some(&mut out_buffer)) }.map_err(text)?;
        let out_buffer = out_buffer.ok_or("the output buffer was not created")?;

        let uav_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: buffer_bytes / 4,
                    Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
                },
            },
        };
        let mut out_uav = None;
        unsafe {
            device.CreateUnorderedAccessView(&out_buffer, Some(&uav_desc), Some(&mut out_uav))
        }
        .map_err(text)?;

        let staging_desc = D3D11_BUFFER_DESC {
            ByteWidth: buffer_bytes,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut staging = None;
        unsafe { device.CreateBuffer(&staging_desc, None, Some(&mut staging)) }.map_err(text)?;
        let staging = staging.ok_or("the staging buffer was not created")?;

        let constants_desc = D3D11_BUFFER_DESC {
            ByteWidth: ShaderParams::SIZE_IN_BYTES as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut constants = None;
        unsafe { device.CreateBuffer(&constants_desc, None, Some(&mut constants)) }
            .map_err(text)?;

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 16,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: -f32::MAX,
            MaxLOD: f32::MAX,
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }.map_err(text)?;

        Ok(Self {
            device: device.clone(),
            context,
            format,
            dst_width,
            dst_height,
            frame_bytes,
            output: Output::Memory {
                shader,
                buffer: out_buffer,
                view: out_uav,
                staging,
            },
            constants,
            sampler,
            source: None,
            cursor: None,
        })
    }

    /// Zero copy (design.md section 2.8): from now on the shader writes into the planes of `ring`'s textures
    /// ([`convert_into`](Self::convert_into)) instead of the buffer read back into memory, which is released.
    pub(crate) fn use_surfaces(&mut self, ring: Arc<SurfaceRing>) -> Result<(), String> {
        let (entry, luma, chroma) = match self.format {
            CaptureFormat::P010 => (
                shader::ENTRY_P010_PLANES,
                DXGI_FORMAT_R16_UINT,
                DXGI_FORMAT_R16G16_UINT,
            ),
            CaptureFormat::Nv12 => (
                shader::ENTRY_NV12_PLANES,
                DXGI_FORMAT_R8_UINT,
                DXGI_FORMAT_R8G8_UINT,
            ),
            CaptureFormat::Auto => return Err("format must be resolved".to_owned()),
        };
        let layout = ring.layout();
        if (layout.format, layout.width, layout.height)
            != (self.format, self.dst_width, self.dst_height)
        {
            return Err("the surfaces do not match the frames".to_owned());
        }
        let bytecode = compile_shader(shader::source_bytes(), entry)
            .map_err(|e| format!("conversion shader failed to compile: {e}"))?;
        let mut compiled = None;
        // SAFETY: plain object creation on a live device; the out pointer is a valid local.
        unsafe {
            self.device
                .CreateComputeShader(&bytecode, None, Some(&mut compiled))
        }
        .map_err(|e| error_text(&e))?;
        let shader = compiled.ok_or("the compute shader was not created")?;
        let context = self
            .context
            .cast::<ID3D11DeviceContext4>()
            .map_err(|_| "the GPU driver has no Direct3D 11.4 fences".to_owned())?;
        let mut views = Vec::with_capacity(ring.textures().len());
        for texture in ring.textures() {
            views.push([
                plane_view(&self.device, texture, luma, "luma")?,
                plane_view(&self.device, texture, chroma, "chroma")?,
            ]);
        }
        self.output = Output::Surfaces {
            shader,
            ring,
            views,
            context,
        };
        Ok(())
    }

    pub(crate) fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub(crate) fn has_source(&self) -> bool {
        self.source.is_some()
    }

    fn create_texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        initial: Option<&D3D11_SUBRESOURCE_DATA>,
    ) -> Result<(ID3D11Texture2D, Option<ID3D11ShaderResourceView>), String> {
        create_shader_texture(&self.device, width, height, format, initial)
    }

    /// Copies the duplicated desktop texture into our own shader-readable texture.
    pub(crate) fn update_source(&mut self, desktop: &ID3D11Texture2D) -> Result<(), String> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain COM call writing into a valid local.
        unsafe { desktop.GetDesc(&mut d) };
        let reusable = self
            .source
            .as_ref()
            .is_some_and(|s| s.width == d.Width && s.height == d.Height && s.format == d.Format);
        if !reusable {
            self.source = None;
            let (texture, view) = self.create_texture(d.Width, d.Height, d.Format, None)?;
            self.source = Some(SourceCopy {
                texture,
                view,
                width: d.Width,
                height: d.Height,
                format: d.Format,
            });
        }
        if let Some(source) = &self.source {
            // SAFETY: both textures live on this device and have the same size and format.
            unsafe { self.context.CopyResource(&source.texture, desktop) };
        }
        Ok(())
    }

    /// Uploads a decoded cursor image (BGRA, straight alpha; a=0 && r=1 means "invert desktop").
    pub(crate) fn set_cursor(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        if bgra.len() < width as usize * height as usize * 4 {
            return Err("cursor image too small".to_owned());
        }
        let reusable = self
            .cursor
            .as_ref()
            .is_some_and(|c| c.width == width && c.height == height);
        if !reusable {
            self.cursor = None;
            let (texture, view) =
                self.create_texture(width, height, DXGI_FORMAT_B8G8R8A8_UNORM, None)?;
            self.cursor = Some(CursorTexture {
                texture,
                view,
                width,
                height,
            });
        }
        if let Some(cursor) = &self.cursor {
            // SAFETY: `bgra` holds at least height rows of width*4 bytes (checked above), which is what the
            // pitches describe for the whole subresource.
            unsafe {
                self.context.UpdateSubresource(
                    &cursor.texture,
                    0,
                    None,
                    bgra.as_ptr().cast(),
                    width * 4,
                    width * 4 * height,
                );
            }
        }
        Ok(())
    }

    /// Runs the shader and copies the finished frame into `destination`.
    pub(crate) fn convert(
        &mut self,
        parameters: &ShaderParams,
        destination: &mut [u8],
    ) -> Result<(), String> {
        let Output::Memory {
            shader,
            buffer,
            view,
            staging,
        } = &self.output
        else {
            return Err("the converter writes into the encoder's surfaces".to_owned());
        };
        if destination.len() < self.frame_bytes {
            return Err("destination buffer too small".to_owned());
        }
        let groups = if self.format == CaptureFormat::P010 {
            self.block_groups()
        } else {
            // main_nv12 is a 1-D kernel folded into a 2-D dispatch of NV12_GROUPS_X groups per row.
            // Same rounding as the shader: the UV plane can end on a half uint.
            let pixels = u64::from(self.dst_width) * u64::from(self.dst_height);
            let slots = pixels / 4 + pixels.div_ceil(8);
            let groups = slots.div_ceil(u64::from(shader::NV12_GROUP_SIZE));
            let gx = shader::NV12_GROUPS_X;
            (gx, (groups.div_ceil(u64::from(gx)) as u32).max(1))
        };
        self.run(parameters, shader, 0, std::slice::from_ref(view), groups)?;
        let ctx = &self.context;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both buffers live on this device; `staging` is a CPU-readable staging buffer of at least
        // `frame_bytes` bytes. Map blocks until the GPU finished the copy; the mapping stays valid until Unmap,
        // which follows the copy directly (nothing in between can return early), and `destination` holds at
        // least `frame_bytes` bytes.
        unsafe {
            ctx.CopyResource(staging, buffer);
            ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| error_text(&e))?;
            std::ptr::copy_nonoverlapping(
                mapped.pData.cast::<u8>(),
                destination.as_mut_ptr(),
                self.frame_bytes,
            );
            ctx.Unmap(staging, 0);
        }
        Ok(())
    }

    /// Zero copy: runs the shader into surface `surface` and signals the fence the encoder waits for. Nothing
    /// waits here: the work is on its way to the GPU when this returns.
    pub(crate) fn convert_into(
        &mut self,
        parameters: &ShaderParams,
        surface: usize,
    ) -> Result<(), String> {
        let Output::Surfaces {
            shader,
            ring,
            views,
            context,
        } = &self.output
        else {
            return Err("the converter writes into memory".to_owned());
        };
        let views = views.get(surface).ok_or("no such surface")?;
        self.run(parameters, shader, 1, views, self.block_groups())?;
        ring.written(context, surface)
    }

    /// Zero copy: fills surface `surface` with black (limited range).
    pub(crate) fn clear_surface(&mut self, surface: usize) -> Result<(), String> {
        let Output::Surfaces {
            ring,
            views,
            context,
            ..
        } = &self.output
        else {
            return Err("the converter writes into memory".to_owned());
        };
        let [luma, chroma] = views.get(surface).ok_or("no such surface")?;
        let (y, c) = if self.format == CaptureFormat::P010 {
            (
                u32::from(color::to_p010_sample(64)),
                u32::from(color::to_p010_sample(512)),
            )
        } else {
            (16, 128)
        };
        // SAFETY: both views belong to this device and live as long as `self`; the integer views take the values
        // as they are.
        unsafe {
            if let Some(luma) = luma {
                self.context
                    .ClearUnorderedAccessViewUint(luma, &[y, 0, 0, 0]);
            }
            if let Some(chroma) = chroma {
                self.context
                    .ClearUnorderedAccessViewUint(chroma, &[c, c, 0, 0]);
            }
        }
        ring.written(context, surface)
    }

    /// Zero copy: the frame in surface `surface`, read back in [`convert`](Self::convert)'s layout (for the
    /// self-test).
    pub(crate) fn read_surface(&self, surface: usize) -> Result<Vec<u8>, String> {
        let Output::Surfaces { ring, .. } = &self.output else {
            return Err("the converter writes into memory".to_owned());
        };
        let texture = ring.textures().get(surface).ok_or("no such surface")?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: plain COM call writing into a valid local.
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        // SAFETY: plain object creation; `desc` is a valid local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(|e| error_text(&e))?;
        let staging = staging.ok_or("the staging texture was not created")?;
        let row = self.frame_bytes / (self.dst_height as usize * 3 / 2);
        let rows = self.dst_height as usize * 3 / 2;
        let mut frame = Vec::with_capacity(self.frame_bytes);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: both textures live on this device and have the same description apart from usage. A mapped
        // planar texture is `RowPitch` bytes per row, the luma rows followed by the chroma rows (half as many),
        // each at least `row` bytes; the mapping stays valid until Unmap, which follows the reads directly.
        unsafe {
            self.context.CopyResource(&staging, texture);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| error_text(&e))?;
            for y in 0..rows {
                let line = mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize);
                frame.extend_from_slice(std::slice::from_raw_parts(line, row));
            }
            self.context.Unmap(&staging, 0);
        }
        Ok(frame)
    }

    /// Dispatch size of the entry points that run one thread per 2x2 block.
    fn block_groups(&self) -> (u32, u32) {
        let g = shader::P010_GROUP_SIZE;
        (
            self.dst_width.div_ceil(2).div_ceil(g),
            self.dst_height.div_ceil(2).div_ceil(g),
        )
    }

    /// Runs `shader` over the source with `views` bound from UAV slot `first_view` on.
    fn run(
        &self,
        parameters: &ShaderParams,
        shader: &ID3D11ComputeShader,
        first_view: u32,
        views: &[Option<ID3D11UnorderedAccessView>],
        (groups_x, groups_y): (u32, u32),
    ) -> Result<(), String> {
        let Some(source) = &self.source else {
            return Err("no source texture".to_owned());
        };
        let mut parameters = *parameters;
        let cursor_view = self.cursor.as_ref().and_then(|c| c.view.clone());
        if cursor_view.is_none() {
            parameters.flags &= !shader::FLAG_CURSOR_VISIBLE;
        }
        let ctx = &self.context;
        let unbound: Vec<Option<ID3D11UnorderedAccessView>> = vec![None; views.len()];

        // SAFETY: every resource bound here was created on this device and is kept alive by `self` for the
        // duration of the calls; `parameters` is a 64-byte block matching the constant buffer's size; the view
        // arrays hold `views.len()` entries.
        unsafe {
            if let Some(constants) = &self.constants {
                ctx.UpdateSubresource(constants, 0, None, (&raw const parameters).cast(), 0, 0);
            }
            ctx.CSSetShader(shader, None);
            ctx.CSSetConstantBuffers(0, Some(std::slice::from_ref(&self.constants)));
            ctx.CSSetShaderResources(0, Some(&[source.view.clone(), cursor_view]));
            ctx.CSSetSamplers(0, Some(std::slice::from_ref(&self.sampler)));
            ctx.CSSetUnorderedAccessViews(
                first_view,
                views.len() as u32,
                Some(views.as_ptr()),
                None,
            );
            ctx.Dispatch(groups_x, groups_y, 1);
            // Unbind so the resources can be copied / re-bound next frame.
            ctx.CSSetUnorderedAccessViews(
                first_view,
                views.len() as u32,
                Some(unbound.as_ptr()),
                None,
            );
            ctx.CSSetShaderResources(0, Some(&[None, None]));
        }
        Ok(())
    }
}

/// A typed view of one plane of a planar (P010 / NV12) texture: a one-channel format selects the luma plane,
/// a two-channel one the chroma plane.
fn plane_view(
    device: &ID3D11Device,
    texture: &ID3D11Texture2D,
    format: DXGI_FORMAT,
    plane: &str,
) -> Result<Option<ID3D11UnorderedAccessView>, String> {
    let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: format,
        ViewDimension: D3D11_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_UAV { MipSlice: 0 },
        },
    };
    let mut view = None;
    // SAFETY: plain object creation on a live device; `desc` and the out pointer are valid locals.
    unsafe { device.CreateUnorderedAccessView(texture, Some(&desc), Some(&mut view)) }.map_err(
        |e| {
            format!(
                "the shader cannot write the {plane} plane of the encoder's surface: {}",
                error_text(&e)
            )
        },
    )?;
    Ok(view)
}

/// A shader-readable 2-D texture (one mip, no multisampling) and its view.
pub(crate) fn create_shader_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    initial: Option<&D3D11_SUBRESOURCE_DATA>,
) -> Result<(ID3D11Texture2D, Option<ID3D11ShaderResourceView>), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: plain D3D11 object creation; `desc` and the optional initial data (whose memory the caller
    // keeps alive for this call) are valid.
    unsafe { device.CreateTexture2D(&desc, initial.map(std::ptr::from_ref), Some(&mut texture)) }
        .map_err(|e| error_text(&e))?;
    let texture = texture.ok_or("the texture was not created")?;
    let mut view = None;
    // SAFETY: as above; the view covers the whole texture.
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut view)) }
        .map_err(|e| error_text(&e))?;
    Ok((texture, view))
}
