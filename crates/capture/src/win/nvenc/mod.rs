//! NVENC driven by the engine itself (docs/design.md §2.8), through the driver's nvEncodeAPI64.dll: an HEVC
//! session, synchronous, with a keyframe exactly when the engine says so. Fed one of two ways:
//!
//! * zero copy, when the screen is on the NVIDIA GPU: the session lives on the capture device and reads the
//!   P010 / NV12 textures the conversion shader wrote ([`SurfaceRing`]), registered once and mapped per picture
//!   once the fence says the shader is done;
//! * otherwise from system memory (the frames the converter read back), on a Direct3D 11 device of its own on
//!   the NVIDIA adapter.
//!
//! The settings follow what ffmpeg's hevc_nvenc gets from `encoder_profiles` (preset, `-tune hq`, CBR, VBV,
//! multipass, spatial AQ, B-frames as references in the middle, Main10 / Main, high tier, the VUI colour tags,
//! one slice per picture, parameter sets before every IDR), with two differences on purpose: no lookahead (it
//! would hold every frame, the asked-for keyframe included, 20 frames back), and no keyframe interval in the
//! encoder (the engine's [`KeyframeClock`](crate::encode::KeyframeClock) forces each one).

mod ffi;

use std::ffi::{c_void, CStr};
use std::sync::Arc;
use std::time::Duration;

use windows::core::{w, Interface};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_DESC, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Threading::CreateEventW;

use self::ffi::*;
use super::surfaces::SurfaceRing;
use super::util::{error_text, wide_to_string, Handle};
use crate::args::{CaptureFormat, EncodeArgs, Multipass};
use crate::encode::{EncodedPicture, VideoEncoder};
use crate::frame::SharedFrame;
use crate::platform::FrameLayout;

const NVIDIA_VENDOR_ID: u32 = 0x10DE;

/// How long the encoder waits for the shader to finish a picture: past the GPU's own hang timeout (2 s), after
/// which the device is reset anyway.
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(3);

/// nvEncodeAPI64.dll and its function table; the library stays loaded while this lives.
struct Api {
    module: HMODULE,
    list: NV_ENCODE_API_FUNCTION_LIST,
}

impl Api {
    fn load() -> Result<Api, String> {
        // From System32 only: the DLL comes with the driver, and a copy anywhere else is not one to trust.
        // SAFETY: plain call with a static, NUL-terminated name.
        let module = unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
            .map_err(|_| "there is no NVIDIA video driver with NVENC on this PC (nvEncodeAPI64.dll)".to_owned())?;
        let api = Api { module, list: zeroed() };
        // SAFETY: the names are NUL-terminated; a found symbol has the documented signature (nvEncodeAPI.h).
        let (max_version, create) = unsafe {
            (
                GetProcAddress(module, windows::core::s!("NvEncodeAPIGetMaxSupportedVersion")),
                GetProcAddress(module, windows::core::s!("NvEncodeAPICreateInstance")),
            )
        };
        let (Some(max_version), Some(create)) = (max_version, create) else {
            return Err("nvEncodeAPI64.dll lacks its entry points".to_owned());
        };
        type Export = unsafe extern "system" fn() -> isize;
        // SAFETY: both are the documented exports, transmuted to their declared signatures.
        let (max_version, create) = unsafe {
            (
                std::mem::transmute::<Export, NvEncodeAPIGetMaxSupportedVersion>(max_version),
                std::mem::transmute::<Export, NvEncodeAPICreateInstance>(create),
            )
        };
        let mut version = 0u32;
        // SAFETY: the out pointer is a valid local.
        check(unsafe { max_version(&mut version) }, "NvEncodeAPIGetMaxSupportedVersion")?;
        if version < NVENCAPI_REQUIRED_MAX_VERSION {
            return Err(format!(
                "the NVIDIA driver is too old for NVENC API {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION} (it has {}.{}): update the driver",
                version >> 4,
                version & 0xF
            ));
        }
        let mut api = api;
        api.list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        // SAFETY: the function list is zeroed apart from its version, as the API requires.
        check(unsafe { create(&mut api.list) }, "NvEncodeAPICreateInstance")?;
        Ok(api)
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        // SAFETY: the module was loaded by `load`; every session using its functions is gone by now (the
        // encoder holds the Api and destroys its session first).
        unsafe {
            let _ = FreeLibrary(self.module);
        }
    }
}

fn check(status: NVENCSTATUS, what: &str) -> Result<(), String> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", status_name(status)))
    }
}

fn status_name(status: NVENCSTATUS) -> String {
    match status {
        NV_ENC_ERR_NO_ENCODE_DEVICE => "no encode device".to_owned(),
        NV_ENC_ERR_UNSUPPORTED_DEVICE => "unsupported device".to_owned(),
        NV_ENC_ERR_INVALID_PARAM => "invalid parameter".to_owned(),
        NV_ENC_ERR_OUT_OF_MEMORY => "out of memory".to_owned(),
        NV_ENC_ERR_UNSUPPORTED_PARAM => "unsupported parameter".to_owned(),
        NV_ENC_ERR_INVALID_VERSION => "invalid version (driver too old?)".to_owned(),
        NV_ENC_ERR_ENCODER_BUSY => "encoder busy".to_owned(),
        NV_ENC_ERR_GENERIC => "generic error".to_owned(),
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => "too many encode sessions on this GPU".to_owned(),
        other => format!("NVENCSTATUS {other}"),
    }
}

/// The adapter `device` was created on.
fn adapter_of(device: &ID3D11Device) -> Option<DXGI_ADAPTER_DESC> {
    // SAFETY: plain COM calls on a live device and its adapter.
    unsafe { device.cast::<IDXGIDevice>().and_then(|d| d.GetAdapter()).and_then(|a| a.GetDesc()) }.ok()
}

/// Whether `device` is on an NVIDIA GPU, so an NVENC session can live on it (zero copy).
pub(crate) fn is_nvidia_device(device: &ID3D11Device) -> bool {
    adapter_of(device).is_some_and(|desc| desc.VendorId == NVIDIA_VENDOR_ID)
}

/// The first NVIDIA adapter and a Direct3D 11 device on it (the encoder's own: nothing else uses it).
pub(crate) fn nvidia_device() -> Result<(ID3D11Device, String), String> {
    // SAFETY: plain factory creation.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| error_text(&e))?;
    for index in 0.. {
        // SAFETY: plain COM calls on live interfaces; an error ends the enumeration.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else { break };
        let adapter: IDXGIAdapter1 = adapter;
        // SAFETY: as above.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        if desc.VendorId != NVIDIA_VENDOR_ID || desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let mut device = None;
        // SAFETY: plain device creation on a live adapter; the out pointer is a valid local.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .map_err(|e| format!("cannot create a Direct3D 11 device on the NVIDIA GPU: {}", error_text(&e)))?;
        let device = device.ok_or("cannot create a Direct3D 11 device on the NVIDIA GPU")?;
        return Ok((device, wide_to_string(&desc.Description)));
    }
    Err("this PC has no NVIDIA GPU".to_owned())
}

/// An input buffer and the bitstream buffer of the same call.
struct Slot {
    /// Null with zero copy: the pictures come in the surfaces.
    input: *mut c_void,
    output: *mut c_void,
    /// The input buffer's row pitch in bytes, as its lock reported it.
    pitch: u32,
}

/// A surface while calls in flight use it.
#[derive(Clone, Copy)]
struct Mapping {
    input: *mut c_void,
    format: u32,
    calls: u32,
}

/// Zero copy: the converter's surfaces as this session knows them.
struct Surfaces {
    ring: Arc<SurfaceRing>,
    /// Each surface's registration, by index.
    registered: Vec<*mut c_void>,
    /// Each surface's mapping, mapped once while calls use it.
    mapped: Vec<Option<Mapping>>,
    /// The frames of the calls whose pictures have not all come out yet (a run of B-frames): their surfaces stay
    /// mapped and out of the engine's hands until then.
    in_flight: Vec<SharedFrame>,
    /// Signalled by the fence when the shader is done with a surface.
    event: Handle,
}

pub(crate) struct NvencEncoder {
    // Field order is drop order: the session (in Drop) before the surfaces and the device, the device before the
    // library.
    session: Encoder,
    slots: Vec<Slot>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    surfaces: Option<Surfaces>,
    _device: ID3D11Device,
    api: Api,
    gpu: String,
    summary: String,
    width: u32,
    height: u32,
    buffer_format: u32,
    /// Bytes per sample: 2 for p010, 1 for nv12.
    sample_bytes: usize,
    /// Calls handed to nvEncEncodePicture, and how many of their bitstream buffers were read back. A B-frame's
    /// call returns "need more input"; the buffers fill in call order with the pictures in decode order.
    submitted: u64,
    retrieved: u64,
    /// Decode order count of the pictures read back, and how far decode times trail presentation times.
    decoded: i64,
    reorder: i64,
    dynamic_bitrate: bool,
}

// SAFETY: the session, its buffers and the device are used from one thread at a time (built on the engine's
// thread, then used only by the sink's writer thread); NVENC and Direct3D 11 devices allow that. With zero copy
// the device is also the converter's, on the engine's thread at the same time: Direct3D's own lock is on for it
// (`SurfaceRing::new`), and each surface is handed over through the fence one way and the frame pool the other.
unsafe impl Send for NvencEncoder {}

macro_rules! call {
    ($self:expr, $name:ident ( $($arg:expr),* )) => {{
        let f = $self.api.list.$name.ok_or(concat!(stringify!($name), " is missing from the driver"))?;
        // SAFETY: a function of the driver's table, called with this session and parameters valid for the call.
        let status = unsafe { f($self.session, $($arg),*) };
        $self.check(status, stringify!($name))
    }};
}

impl NvencEncoder {
    /// With `device` (zero copy) the session lives on that device and reads the surfaces [`register`]ed next;
    /// without, it gets a device of its own and takes frames in memory.
    ///
    /// [`register`]: Self::register
    pub(crate) fn open(
        args: &EncodeArgs,
        layout: FrameLayout,
        fps: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<NvencEncoder, String> {
        let api = Api::load()?;
        let zero_copy = device.is_some();
        let (device, gpu) = match device {
            Some(device) => {
                let gpu = adapter_of(device).map(|desc| wide_to_string(&desc.Description));
                (device.clone(), gpu.unwrap_or_else(|| "the capture GPU".to_owned()))
            }
            None => nvidia_device()?,
        };
        let hdr = layout.format == CaptureFormat::P010;

        let mut open = zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>();
        open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        open.device = device.as_raw();
        open.apiVersion = NVENCAPI_VERSION;
        let mut session: Encoder = std::ptr::null_mut();
        let open_session = api.list.nvEncOpenEncodeSessionEx.ok_or("nvEncOpenEncodeSessionEx is missing")?;
        // SAFETY: both pointers are valid locals; the device outlives the session (field order).
        check(unsafe { open_session(&mut open, &mut session) }, "opening an NVENC session")?;

        let mut encoder = NvencEncoder {
            session,
            slots: Vec::new(),
            init: Box::new(zeroed()),
            config: Box::new(zeroed()),
            surfaces: None,
            _device: device,
            api,
            gpu,
            summary: String::new(),
            width: layout.width,
            height: layout.height,
            buffer_format: if hdr { NV_ENC_BUFFER_FORMAT_YUV420_10BIT } else { NV_ENC_BUFFER_FORMAT_NV12 },
            sample_bytes: if hdr { 2 } else { 1 },
            submitted: 0,
            retrieved: 0,
            decoded: 0,
            reorder: 0,
            dynamic_bitrate: false,
        };
        encoder.initialize(args, hdr, fps, !zero_copy)?;
        if !zero_copy {
            encoder.summary.push_str(", frames copied in from memory");
        }
        Ok(encoder)
    }

    /// Zero copy: how many pictures the encoder holds back at most (a run of B-frames), on top of the frames the
    /// engine holds.
    pub(crate) fn frames_held(&self) -> usize {
        self.reorder as usize
    }

    /// Zero copy: registers the surfaces (on the device the session was opened on) the pictures come in.
    pub(crate) fn register(&mut self, ring: Arc<SurfaceRing>) -> Result<(), String> {
        // SAFETY: plain event creation: auto-reset, not signalled, unnamed.
        let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| error_text(&e))?;
        let count = ring.textures().len();
        // In place before the first registration, so Drop releases whatever got registered.
        self.surfaces = Some(Surfaces {
            ring: Arc::clone(&ring),
            registered: Vec::with_capacity(count),
            mapped: vec![None; count],
            in_flight: Vec::new(),
            // SAFETY: the event was just created and nothing else owns it.
            event: unsafe { Handle::from_raw(event) },
        });
        for texture in ring.textures() {
            let mut resource = zeroed::<NV_ENC_REGISTER_RESOURCE>();
            resource.version = NV_ENC_REGISTER_RESOURCE_VER;
            resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
            resource.width = self.width;
            resource.height = self.height;
            resource.resourceToRegister = texture.as_raw();
            resource.bufferFormat = self.buffer_format;
            resource.bufferUsage = NV_ENC_INPUT_IMAGE;
            call!(self, nvEncRegisterResource(&mut resource))?;
            if let Some(surfaces) = self.surfaces.as_mut() {
                surfaces.registered.push(resource.registeredResource);
            }
        }
        self.summary.push_str(&format!(", zero copy ({count} surfaces on the GPU)"));
        Ok(())
    }

    fn check(&self, status: NVENCSTATUS, what: &str) -> Result<(), String> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = self
            .api
            .list
            .nvEncGetLastErrorString
            // SAFETY: the driver returns a NUL-terminated string it owns (or null), valid until the next call.
            .map(|f| unsafe { f(self.session) })
            .filter(|p| !p.is_null())
            .map(|p| unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_owned())
            .filter(|s| !s.is_empty());
        Err(match detail {
            Some(detail) => format!("{what} failed: {} ({detail})", status_name(status)),
            None => format!("{what} failed: {}", status_name(status)),
        })
    }

    fn caps(&self, cap: u32) -> i32 {
        let mut param = zeroed::<NV_ENC_CAPS_PARAM>();
        param.version = NV_ENC_CAPS_PARAM_VER;
        param.capsToQuery = cap;
        let mut value = 0;
        let Some(f) = self.api.list.nvEncGetEncodeCaps else { return 0 };
        // SAFETY: valid session and locals.
        let status = unsafe { f(self.session, NV_ENC_CODEC_HEVC_GUID, &mut param, &mut value) };
        if status == NV_ENC_SUCCESS {
            value
        } else {
            0
        }
    }

    /// `input_buffers`: frames come in memory and need NVENC's own input buffers.
    fn initialize(&mut self, args: &EncodeArgs, hdr: bool, fps: u32, input_buffers: bool) -> Result<(), String> {
        if hdr && self.caps(NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) == 0 {
            return Err(format!("{} cannot encode 10-bit HEVC", self.gpu));
        }
        let (max_w, max_h) = (self.caps(NV_ENC_CAPS_WIDTH_MAX), self.caps(NV_ENC_CAPS_HEIGHT_MAX));
        if max_w > 0 && max_h > 0 && (self.width > max_w as u32 || self.height > max_h as u32) {
            return Err(format!("{}x{} is larger than {} encodes ({max_w}x{max_h})", self.width, self.height, self.gpu));
        }
        let b_frames = args.b_frames.min(self.caps(NV_ENC_CAPS_NUM_MAX_BFRAMES).max(0) as u32);
        // 0 none, 1 each, 2 only middle, 3 both
        let middle_refs = b_frames > 1 && self.caps(NV_ENC_CAPS_SUPPORT_BFRAME_REF_MODE) & 2 != 0;
        self.dynamic_bitrate = self.caps(NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE) != 0;

        let preset = NV_ENC_PRESET_GUIDS[(args.preset.clamp(1, 7) - 1) as usize];
        let mut preset_config = Box::new(zeroed::<NV_ENC_PRESET_CONFIG>());
        preset_config.version = NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
        let get_preset = self.api.list.nvEncGetEncodePresetConfigEx.ok_or("nvEncGetEncodePresetConfigEx is missing")?;
        // SAFETY: valid session; the preset config is a live, versioned structure.
        let status = unsafe { get_preset(self.session, NV_ENC_CODEC_HEVC_GUID, preset, NV_ENC_TUNING_INFO_HIGH_QUALITY, &mut *preset_config) };
        self.check(status, "reading the NVENC preset")?;

        let config = &mut *self.config;
        *config = preset_config.presetCfg;
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = if hdr { NV_ENC_HEVC_PROFILE_MAIN10_GUID } else { NV_ENC_HEVC_PROFILE_MAIN_GUID };
        config.gopLength = NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = b_frames as i32 + 1;
        let rc = &mut config.rcParams;
        rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = args.bitrate_kbps.saturating_mul(1000);
        rc.maxBitRate = rc.averageBitRate;
        rc.vbvBufferSize = args.vbv_kbits.saturating_mul(1000);
        rc.vbvInitialDelay = 0;
        rc.multiPass = match args.multipass {
            Multipass::Off => NV_ENC_MULTI_PASS_DISABLED,
            Multipass::Quarter => NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
            Multipass::Full => NV_ENC_TWO_PASS_FULL_RESOLUTION,
        };
        rc.flags &= !(RC_ENABLE_LOOKAHEAD | RC_ENABLE_TEMPORAL_AQ | RC_ENABLE_AQ | RC_AQ_STRENGTH_MASK);
        rc.lookaheadDepth = 0;
        if args.aq_strength > 0 {
            rc.flags |= RC_ENABLE_AQ | (args.aq_strength.min(15) << RC_AQ_STRENGTH_SHIFT);
        }
        // SAFETY: the configuration is for HEVC (the session's codec), so the HEVC member is the live one.
        let hevc = unsafe { &mut config.encodeCodecConfig.hevcConfig };
        hevc.tier = NV_ENC_TIER_HEVC_HIGH;
        hevc.level = 0; // auto
        hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        hevc.flags &= !(HEVC_DISABLE_SPSPPS | HEVC_OUTPUT_AUD | HEVC_CHROMA_FORMAT_MASK | HEVC_BIT_DEPTH_MINUS8_MASK);
        hevc.flags |= HEVC_REPEAT_SPSPPS | (1 << HEVC_CHROMA_FORMAT_SHIFT) | (if hdr { 2 } else { 0 } << HEVC_BIT_DEPTH_MINUS8_SHIFT);
        // one slice per picture, as ffmpeg asks for it (the viewer's guard relies on whole pictures)
        hevc.sliceMode = 3;
        hevc.sliceModeData = 1;
        hevc.useBFramesAsRef = if middle_refs { NV_ENC_BFRAME_REF_MODE_MIDDLE } else { NV_ENC_BFRAME_REF_MODE_DISABLED };
        let vui = &mut hevc.hevcVUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        (vui.colourPrimaries, vui.transferCharacteristics, vui.colourMatrix) = if hdr {
            (NV_ENC_VUI_COLOR_PRIMARIES_BT2020, NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084, NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL)
        } else {
            (NV_ENC_VUI_COLOR_PRIMARIES_BT709, NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709, NV_ENC_VUI_MATRIX_COEFFS_BT709)
        };
        // chroma sited left (as `-chroma_sample_location left`), which is where the converter puts it
        vui.chromaSampleLocationFlag = 1;
        vui.chromaSampleLocationTop = 0;
        vui.chromaSampleLocationBot = 0;

        let init = &mut *self.init;
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_HEVC_GUID;
        init.presetGUID = preset;
        init.encodeWidth = self.width;
        init.encodeHeight = self.height;
        init.darWidth = self.width;
        init.darHeight = self.height;
        init.frameRateNum = fps.max(1);
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        init.encodeConfig = &mut *self.config;
        init.maxEncodeWidth = self.width;
        init.maxEncodeHeight = self.height;
        init.tuningInfo = NV_ENC_TUNING_INFO_HIGH_QUALITY;
        let init_ptr: *mut NV_ENC_INITIALIZE_PARAMS = &mut *self.init;
        call!(self, nvEncInitializeEncoder(init_ptr))?;

        self.reorder = i64::from(b_frames);
        // enough that a slot is read back before it comes round again: the pictures a B-frame run holds, plus room
        for _ in 0..b_frames + 4 {
            let mut input = zeroed::<NV_ENC_CREATE_INPUT_BUFFER>();
            if input_buffers {
                input.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
                input.width = self.width;
                input.height = self.height;
                input.memoryHeap = NV_ENC_MEMORY_HEAP_AUTOSELECT;
                input.bufferFmt = self.buffer_format;
                call!(self, nvEncCreateInputBuffer(&mut input))?;
            }
            let mut output = zeroed::<NV_ENC_CREATE_BITSTREAM_BUFFER>();
            output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            if let Err(e) = call!(self, nvEncCreateBitstreamBuffer(&mut output)) {
                self.slots.push(Slot { input: input.inputBuffer, output: std::ptr::null_mut(), pitch: 0 });
                return Err(e);
            }
            self.slots.push(Slot { input: input.inputBuffer, output: output.bitstreamBuffer, pitch: 0 });
        }

        self.summary = format!(
            "NVENC on {}, HEVC {} {}x{}@{fps}, P{} high quality, {} kbit/s CBR, buffer {} kbit, {} B-frame{}{}, two-pass {}, AQ {}, no lookahead, keyframes every {} frames and on request{}",
            self.gpu,
            if hdr { "Main10 (PQ, BT.2020)" } else { "Main (BT.709)" },
            self.width,
            self.height,
            args.preset.clamp(1, 7),
            args.bitrate_kbps,
            args.vbv_kbits,
            b_frames,
            if b_frames == 1 { "" } else { "s" },
            if middle_refs { " (middle as reference)" } else { "" },
            args.multipass.name(),
            if args.aq_strength > 0 { args.aq_strength.to_string() } else { "off".to_owned() },
            args.gop_frames,
            if self.dynamic_bitrate { ", bitrate changeable while running" } else { "" },
        );
        Ok(())
    }

    /// Zero copy: the input for the picture in the surface `frame` stands for. Maps the surface (once while
    /// calls use it) when the shader is done with it, and keeps `frame` until the picture is out.
    fn map_surface(&mut self, frame: &SharedFrame) -> Result<(*mut c_void, u32), String> {
        let surface = frame.index();
        let (registered, mapping) = match &self.surfaces {
            Some(surfaces) if surface < surfaces.registered.len() => {
                (surfaces.registered[surface], surfaces.mapped[surface])
            }
            _ => return Err(format!("the frame names surface {surface}, which this encoder does not have")),
        };
        let mapping = match mapping {
            Some(mapping) => mapping,
            None => {
                if let Some(surfaces) = &self.surfaces {
                    surfaces.ring.wait_written(surface, surfaces.event.raw(), CONVERSION_TIMEOUT)?;
                }
                let mut map = zeroed::<NV_ENC_MAP_INPUT_RESOURCE>();
                map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
                map.registeredResource = registered;
                call!(self, nvEncMapInputResource(&mut map))?;
                Mapping { input: map.mappedResource, format: map.mappedBufferFmt, calls: 0 }
            }
        };
        if let Some(surfaces) = self.surfaces.as_mut() {
            surfaces.mapped[surface] = Some(Mapping { calls: mapping.calls + 1, ..mapping });
            surfaces.in_flight.push(Arc::clone(frame));
        }
        Ok((mapping.input, mapping.format))
    }

    /// Zero copy: every call so far has its picture out, so their surfaces are unmapped and given back.
    fn release_in_flight(&mut self) -> Result<(), String> {
        let Some(surfaces) = self.surfaces.as_mut() else { return Ok(()) };
        let frames = std::mem::take(&mut surfaces.in_flight);
        let mut unmap = Vec::new();
        for frame in &frames {
            let slot = &mut surfaces.mapped[frame.index()];
            if let Some(mapping) = slot.as_mut() {
                mapping.calls -= 1;
                if mapping.calls == 0 {
                    unmap.push(mapping.input);
                    *slot = None;
                }
            }
        }
        for input in unmap {
            call!(self, nvEncUnmapInputResource(input))?;
        }
        // only now: the engine may convert into these surfaces again
        drop(frames);
        Ok(())
    }

    fn copy_in(&mut self, slot: usize, frame: &[u8]) -> Result<(), String> {
        let row = self.width as usize * self.sample_bytes;
        let (luma_rows, chroma_rows) = (self.height as usize, self.height as usize / 2);
        if frame.len() < row * (luma_rows + chroma_rows) {
            return Err("the frame is smaller than its layout".to_owned());
        }
        let mut lock = zeroed::<NV_ENC_LOCK_INPUT_BUFFER>();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.inputBuffer = self.slots[slot].input;
        call!(self, nvEncLockInputBuffer(&mut lock))?;
        let pitch = lock.pitch as usize;
        let base = lock.bufferDataPtr.cast::<u8>();
        if pitch >= row && !base.is_null() {
            // SAFETY: a locked NV12 / P010 input buffer is `pitch` bytes per row, the luma rows followed by the
            // chroma rows (half as many) at `pitch * height`; each copy stays inside its row and inside `frame`
            // (checked above). The buffer stays locked until the unlock below.
            unsafe {
                for y in 0..luma_rows {
                    std::ptr::copy_nonoverlapping(frame.as_ptr().add(y * row), base.add(y * pitch), row);
                }
                let chroma = base.add(pitch * luma_rows);
                let source = frame.as_ptr().add(row * luma_rows);
                for y in 0..chroma_rows {
                    std::ptr::copy_nonoverlapping(source.add(y * row), chroma.add(y * pitch), row);
                }
            }
        }
        self.slots[slot].pitch = lock.pitch;
        let input = self.slots[slot].input;
        call!(self, nvEncUnlockInputBuffer(input))?;
        if pitch < row || base.is_null() {
            return Err(format!("the locked input buffer is unusable (pitch {pitch} for rows of {row} bytes)"));
        }
        Ok(())
    }

    /// Reads back every bitstream buffer up to the last call, in order: the pictures in decode order.
    fn drain(&mut self, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
        while self.retrieved < self.submitted {
            let slot = (self.retrieved % self.slots.len() as u64) as usize;
            let mut lock = zeroed::<NV_ENC_LOCK_BITSTREAM>();
            lock.version = NV_ENC_LOCK_BITSTREAM_VER;
            lock.outputBitstream = self.slots[slot].output;
            call!(self, nvEncLockBitstream(&mut lock))?;
            let size = lock.bitstreamSizeInBytes as usize;
            // SAFETY: while locked, the driver's buffer holds `bitstreamSizeInBytes` bytes at bitstreamBufferPtr.
            let data = if size == 0 || lock.bitstreamBufferPtr.is_null() {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr.cast::<u8>(), size) }.to_vec()
            };
            let output = self.slots[slot].output;
            call!(self, nvEncUnlockBitstream(output))?;
            self.retrieved += 1;
            if data.is_empty() {
                continue;
            }
            out.push(EncodedPicture {
                data,
                pts: lock.outputTimeStamp as i64,
                dts: self.decoded - self.reorder,
                keyframe: lock.pictureType == NV_ENC_PIC_TYPE_IDR,
            });
            self.decoded += 1;
        }
        Ok(())
    }

    fn submit(&mut self, params: &mut NV_ENC_PIC_PARAMS, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
        let f = self.api.list.nvEncEncodePicture.ok_or("nvEncEncodePicture is missing from the driver")?;
        // SAFETY: valid session; the parameters name this session's buffers.
        let status = unsafe { f(self.session, params) };
        self.submitted += 1;
        match status {
            // every call so far is done: the inputs of the B-frames that waited too
            NV_ENC_SUCCESS => {
                self.drain(out)?;
                self.release_in_flight()
            }
            NV_ENC_ERR_NEED_MORE_INPUT => Ok(()),
            other => self.check(other, "nvEncEncodePicture"),
        }
    }
}

impl VideoEncoder for NvencEncoder {
    fn describe(&self) -> String {
        self.summary.clone()
    }

    fn surfaces(&self) -> usize {
        self.surfaces.as_ref().map_or(0, |surfaces| surfaces.registered.len())
    }

    fn encode(&mut self, frame: &SharedFrame, index: u64, keyframe: bool, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
        let slot = (self.submitted % self.slots.len() as u64) as usize;
        let (input, format, pitch) = if self.surfaces.is_some() {
            let (input, format) = self.map_surface(frame)?;
            (input, format, self.width)
        } else {
            self.copy_in(slot, frame)?;
            (self.slots[slot].input, self.buffer_format, self.slots[slot].pitch)
        };
        let mut params = zeroed::<NV_ENC_PIC_PARAMS>();
        params.version = NV_ENC_PIC_PARAMS_VER;
        params.inputWidth = self.width;
        params.inputHeight = self.height;
        params.inputPitch = pitch;
        params.encodePicFlags = if keyframe { NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        params.frameIdx = index as u32;
        params.inputTimeStamp = index;
        params.inputDuration = 1;
        params.inputBuffer = input;
        params.outputBitstream = self.slots[slot].output;
        params.bufferFmt = format;
        params.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        self.submit(&mut params, out)
    }

    fn set_bitrate(&mut self, kbps: u32, vbv_kbits: u32) -> Result<(), String> {
        if !self.dynamic_bitrate {
            return Err(format!("{} cannot change the bitrate while encoding", self.gpu));
        }
        let rc = &mut self.config.rcParams;
        rc.averageBitRate = kbps.saturating_mul(1000);
        rc.maxBitRate = rc.averageBitRate;
        rc.vbvBufferSize = vbv_kbits.saturating_mul(1000);
        let mut params = Box::new(zeroed::<NV_ENC_RECONFIGURE_PARAMS>());
        params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = *self.init;
        params.reInitEncodeParams.encodeConfig = &mut *self.config;
        // neither a reset nor an IDR: the rate control carries on at the new target
        params.flags = 0;
        let params: *mut NV_ENC_RECONFIGURE_PARAMS = &mut *params;
        call!(self, nvEncReconfigureEncoder(params))
    }

    fn finish(&mut self, out: &mut Vec<EncodedPicture>) -> Result<(), String> {
        if self.submitted == self.retrieved && self.submitted == 0 {
            return Ok(());
        }
        let mut params = zeroed::<NV_ENC_PIC_PARAMS>();
        params.version = NV_ENC_PIC_PARAMS_VER;
        params.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
        let f = self.api.list.nvEncEncodePicture.ok_or("nvEncEncodePicture is missing from the driver")?;
        // SAFETY: an end-of-stream call carries no buffers.
        let status = unsafe { f(self.session, &mut params) };
        self.check(status, "ending the NVENC stream")?;
        self.drain(out)?;
        self.release_in_flight()
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let list = self.api.list;
        // SAFETY: the buffers, mappings and registrations belong to this session, which is destroyed last;
        // nothing uses them afterwards. The surfaces themselves outlive the session (field order).
        unsafe {
            if let Some(surfaces) = &self.surfaces {
                if let Some(f) = list.nvEncUnmapInputResource {
                    for mapping in surfaces.mapped.iter().flatten() {
                        f(self.session, mapping.input);
                    }
                }
                if let Some(f) = list.nvEncUnregisterResource {
                    for &registered in &surfaces.registered {
                        f(self.session, registered);
                    }
                }
            }
            for slot in &self.slots {
                if let (Some(f), false) = (list.nvEncDestroyInputBuffer, slot.input.is_null()) {
                    f(self.session, slot.input);
                }
                if let (Some(f), false) = (list.nvEncDestroyBitstreamBuffer, slot.output.is_null()) {
                    f(self.session, slot.output);
                }
            }
            if let (Some(f), false) = (list.nvEncDestroyEncoder, self.session.is_null()) {
                f(self.session);
            }
        }
    }
}
