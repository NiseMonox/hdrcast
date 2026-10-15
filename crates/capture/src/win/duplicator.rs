//! DXGI Desktop Duplication for one output: creates the D3D11 device on the owning adapter, requests an FP16
//! (scRGB, HDR desktop) or BGRA8 (SDR desktop) duplication and feeds frames to the [`GpuConverter`].

use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

use super::converter::GpuConverter;
use super::cursor::{decode_cursor, CursorImage};
use super::display_config;
use super::outputs::{adapter_name, describe, for_each_output};
use super::util::{error_text, hr_text};
use crate::output::OutputInfo;

/// Outcome of one [`DesktopDuplicator::try_acquire`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcquireResult {
    /// A new desktop image was copied into the converter.
    NewFrame,
    /// No new frame within the timeout - the desktop did not change.
    Timeout,
    /// Only the mouse pointer moved / changed shape.
    PointerOnly,
    /// The duplication was invalidated (mode change, full-screen app, session switch): recreate it.
    AccessLost,
}

/// Calls `ReleaseFrame` when dropped: every frame that was acquired is released on every path out.
struct AcquiredFrame<'a>(&'a IDXGIOutputDuplication);

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: a frame is currently acquired on this duplication (that is when the guard is created).
        // An error means already released / access lost, which the next acquire reports.
        let _ = unsafe { self.0.ReleaseFrame() };
    }
}

pub(crate) struct DesktopDuplicator {
    want_hdr: bool,
    device: ID3D11Device,
    output5: IDXGIOutput5,
    duplication: Option<IDXGIOutputDuplication>,
    shape_buffer: Vec<u8>,

    /// Size of the duplicated framebuffer (not rotated).
    pub source_width: u32,
    pub source_height: u32,
    /// Desktop size as the user sees it (rotation applied).
    pub desktop_width: u32,
    pub desktop_height: u32,
    /// DXGI_MODE_ROTATION - 1: 0 identity, 1 = 90 degrees, 2 = 180, 3 = 270.
    pub rotation: u32,
    /// True when the duplication delivers R16G16B16A16_FLOAT (scRGB) frames.
    pub source_is_float: bool,
    pub info: OutputInfo,

    pub cursor_visible: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_width: u32,
    pub cursor_height: u32,
    /// Set when a new cursor bitmap was decoded; the caller uploads it and takes it.
    pending_cursor: Option<CursorImage>,
}

impl DesktopDuplicator {
    /// Creates the device and the duplication for the output with the global index `output_index`.
    /// Fails with a readable message.
    pub(crate) fn open(output_index: u32, want_hdr: bool) -> Result<Self, String> {
        let monitors = display_config::query_active_monitors();
        let mut found = None;
        let mut index = 0u32;
        for_each_output(|adapter, adapter_index, output, output_index_on_adapter| {
            if index != output_index {
                index += 1;
                return Ok(true);
            }
            // The D3D11 device must live on the adapter that owns the output.
            let mut device = None;
            // SAFETY: plain device creation on a live adapter; the out pointer is a valid local.
            unsafe {
                D3D11CreateDevice(
                    adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
            }
            .map_err(|e| format!("cannot create the Direct3D 11 device: {}", error_text(&e)))?;
            let device: ID3D11Device = device.ok_or("cannot create the Direct3D 11 device")?;
            let name = adapter_name(adapter, adapter_index);
            let info = describe(
                output,
                &name,
                adapter_index,
                output_index_on_adapter,
                output_index,
                &monitors,
            )?;
            let output5 = output.cast::<IDXGIOutput5>().map_err(|e| error_text(&e))?;
            found = Some((device, output5, info));
            Ok(false)
        })?;
        let Some((device, output5, info)) = found else {
            return Err(format!(
                "DXGI output {output_index} does not exist (run list-outputs)"
            ));
        };

        let (w, h) = (info.width.max(0) as u32, info.height.max(0) as u32);
        let rotation = (info.rotation - 1).max(0) as u32;
        // A 90/270 rotated desktop is duplicated in the framebuffer's (unrotated) orientation.
        let swapped = rotation == 1 || rotation == 3;
        let mut duplicator = Self {
            want_hdr,
            device,
            output5,
            duplication: None,
            shape_buffer: Vec::new(),
            source_width: if swapped { h } else { w },
            source_height: if swapped { w } else { h },
            desktop_width: w,
            desktop_height: h,
            rotation,
            source_is_float: false,
            info,
            cursor_visible: false,
            cursor_x: 0,
            cursor_y: 0,
            cursor_width: 0,
            cursor_height: 0,
            pending_cursor: None,
        };
        duplicator.create_duplication()?;
        Ok(duplicator)
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    fn create_duplication(&mut self) -> Result<(), String> {
        // Only one duplication of an output can exist per process: let go of the old one first.
        self.duplication = None;

        // Ask for FP16 first on an HDR desktop: that is the only format carrying values above SDR white.
        const HDR_FORMATS: &[DXGI_FORMAT] =
            &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];
        const SDR_FORMATS: &[DXGI_FORMAT] = &[DXGI_FORMAT_B8G8R8A8_UNORM];
        let formats = if self.want_hdr {
            HDR_FORMATS
        } else {
            SDR_FORMATS
        };
        // SAFETY: plain COM call on live interfaces.
        let duplication = unsafe { self.output5.DuplicateOutput1(&self.device, 0, formats) }.map_err(|e| {
            let code = e.code();
            if code == E_ACCESSDENIED {
                format!(
                    "Desktop Duplication was denied (another capture session, or a protected full-screen app). HRESULT {}",
                    hr_text(code)
                )
            } else if code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE {
                format!("Too many desktop duplications are already active on this GPU. HRESULT {}", hr_text(code))
            } else if code == DXGI_ERROR_UNSUPPORTED || code == DXGI_ERROR_INVALID_CALL {
                format!("This display cannot be duplicated in the requested format. HRESULT {}", hr_text(code))
            } else {
                error_text(&e)
            }
        })?;

        // SAFETY: plain COM call returning a value.
        let desc = unsafe { duplication.GetDesc() };
        self.source_is_float = desc.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        if desc.ModeDesc.Width > 0 && desc.ModeDesc.Height > 0 {
            self.source_width = desc.ModeDesc.Width;
            self.source_height = desc.ModeDesc.Height;
        }
        self.duplication = Some(duplication);
        Ok(())
    }

    /// Recreates the duplication after DXGI_ERROR_ACCESS_LOST (device stays alive).
    pub(crate) fn recreate(&mut self) -> Result<(), String> {
        self.create_duplication()
    }

    /// Tries to get one frame. On [`AcquireResult::NewFrame`] the image was copied into `converter`; pointer
    /// state is updated on every call that returns a frame or pointer update.
    pub(crate) fn try_acquire(
        &mut self,
        timeout_ms: u32,
        converter: &mut GpuConverter,
    ) -> Result<AcquireResult, String> {
        // A second reference, so the pointer state of `self` can be updated while the frame is held.
        let Some(duplication) = self.duplication.clone() else {
            return Ok(AcquireResult::AccessLost);
        };

        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: plain COM call; both out pointers are valid locals.
        if let Err(e) =
            unsafe { duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) }
        {
            let code = e.code();
            if code == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(AcquireResult::Timeout);
            }
            if code == DXGI_ERROR_ACCESS_LOST || code == DXGI_ERROR_SESSION_DISCONNECTED {
                return Ok(AcquireResult::AccessLost);
            }
            return Err(error_text(&e));
        }
        // From here a frame is held. Declared in this order so that on every path out the desktop resource
        // is released first and the frame second.
        let _frame = AcquiredFrame(&duplication);
        let resource = resource;

        self.update_pointer(&duplication, &frame_info);
        let new_image = frame_info.LastPresentTime != 0 && frame_info.AccumulatedFrames > 0;
        if new_image {
            if let Some(resource) = &resource {
                let texture = resource
                    .cast::<ID3D11Texture2D>()
                    .map_err(|e| error_text(&e))?;
                converter.update_source(&texture)?;
            }
        }
        Ok(if new_image {
            AcquireResult::NewFrame
        } else if frame_info.LastMouseUpdateTime != 0 {
            AcquireResult::PointerOnly
        } else {
            AcquireResult::Timeout
        })
    }

    fn update_pointer(
        &mut self,
        duplication: &IDXGIOutputDuplication,
        frame_info: &DXGI_OUTDUPL_FRAME_INFO,
    ) {
        if frame_info.LastMouseUpdateTime != 0 {
            self.cursor_visible = frame_info.PointerPosition.Visible.as_bool();
            self.cursor_x = frame_info.PointerPosition.Position.x;
            self.cursor_y = frame_info.PointerPosition.Position.y;
        }
        let needed = frame_info.PointerShapeBufferSize as usize;
        if needed == 0 {
            return;
        }
        if self.shape_buffer.len() < needed {
            self.shape_buffer.resize(needed, 0);
        }

        let mut required = 0u32;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        // SAFETY: the buffer is valid for the size passed; the out pointers are valid locals. A frame is
        // acquired (the caller holds it), which is when the shape can be read.
        let read = unsafe {
            duplication.GetFramePointerShape(
                self.shape_buffer.len() as u32,
                self.shape_buffer.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
            )
        };
        // A shape we cannot read is not fatal; keep the previous cursor image.
        if read.is_err() {
            return;
        }
        if let Some(image) = decode_cursor(
            &self.shape_buffer,
            shape.Type,
            shape.Width,
            shape.Height,
            shape.Pitch,
        ) {
            self.cursor_width = image.width;
            self.cursor_height = image.height;
            self.pending_cursor = Some(image);
        }
    }

    /// The cursor image decoded since the last call, if any.
    pub(crate) fn take_pending_cursor(&mut self) -> Option<CursorImage> {
        self.pending_cursor.take()
    }
}
