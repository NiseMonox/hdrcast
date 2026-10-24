//! The part of NVIDIA's NVENCODE API 12.1 (`nvEncodeAPI.h`, MIT licence; the driver's nvEncodeAPI64.dll) the
//! engine uses: a session on a Direct3D 11 device with input buffers in system memory or registered Direct3D 11
//! textures (zero copy), HEVC, its function table, and the constants that go with them.
//!
//! Written by hand from the header, not generated. Every structure's size, the offset of every field and the bit
//! of every bitfield used here were printed from the header by a program built with MSVC (2026-10-24) and are
//! asserted at the end of this file, so a mistake in here cannot compile. Fields the engine never touches are
//! folded into the reserved arrays that follow them, at the same offsets.

// The declarations below are taken from nvEncodeAPI.h 12.1, whose notice reads:
//
// Copyright (c) 2010-2023 NVIDIA Corporation
//
// Permission is hereby granted, free of charge, to any person
// obtaining a copy of this software and associated documentation
// files (the "Software"), to deal in the Software without
// restriction, including without limitation the rights to use,
// copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the software, and to permit persons to whom the
// software is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice shall be
// included in all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
// OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
// NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
// HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.

#![allow(non_camel_case_types, non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_int, c_void};
use std::mem::{offset_of, size_of};

pub use windows::core::GUID;

pub type NVENCSTATUS = i32;
pub const NV_ENC_SUCCESS: NVENCSTATUS = 0;
pub const NV_ENC_ERR_NO_ENCODE_DEVICE: NVENCSTATUS = 1;
pub const NV_ENC_ERR_UNSUPPORTED_DEVICE: NVENCSTATUS = 2;
pub const NV_ENC_ERR_INVALID_PARAM: NVENCSTATUS = 8;
pub const NV_ENC_ERR_OUT_OF_MEMORY: NVENCSTATUS = 10;
pub const NV_ENC_ERR_UNSUPPORTED_PARAM: NVENCSTATUS = 12;
pub const NV_ENC_ERR_INVALID_VERSION: NVENCSTATUS = 15;
pub const NV_ENC_ERR_MAP_FAILED: NVENCSTATUS = 16;
pub const NV_ENC_ERR_NEED_MORE_INPUT: NVENCSTATUS = 17;
pub const NV_ENC_ERR_ENCODER_BUSY: NVENCSTATUS = 18;
pub const NV_ENC_ERR_GENERIC: NVENCSTATUS = 20;
pub const NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY: NVENCSTATUS = 21;
pub const NV_ENC_ERR_RESOURCE_REGISTER_FAILED: NVENCSTATUS = 23;

pub const NVENCAPI_MAJOR_VERSION: u32 = 12;
pub const NVENCAPI_MINOR_VERSION: u32 = 1;
pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
/// What `NvEncodeAPIGetMaxSupportedVersion` must report at least for this header: major << 4 | minor.
pub const NVENCAPI_REQUIRED_MAX_VERSION: u32 = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;

pub const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}

pub const NV_ENC_CAPS_PARAM_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_CONFIG_VER: u32 = struct_version(8) | (1 << 31);
pub const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
pub const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(1) | (1 << 31);
pub const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const NV_ENC_MAP_INPUT_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_REGISTER_RESOURCE_VER: u32 = struct_version(4);
pub const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

pub const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;

pub const NV_ENC_CODEC_HEVC_GUID: GUID = GUID::from_values(0x790cdc88, 0x4522, 0x4d7b, [0x94, 0x25, 0xbd, 0xa9, 0x97, 0x5f, 0x76, 0x03]);
pub const NV_ENC_HEVC_PROFILE_MAIN_GUID: GUID = GUID::from_values(0xb514c39a, 0xb55b, 0x40fa, [0x87, 0x8f, 0xf1, 0x25, 0x3b, 0x4d, 0xfd, 0xec]);
pub const NV_ENC_HEVC_PROFILE_MAIN10_GUID: GUID = GUID::from_values(0xfa4d2b6c, 0x3a5b, 0x411a, [0x80, 0x18, 0x0a, 0x3f, 0x5e, 0x3c, 0x9b, 0xe5]);
/// Presets P1 (fastest) .. P7 (best), in that order.
pub const NV_ENC_PRESET_GUIDS: [GUID; 7] = [
    GUID::from_values(0xfc0a8d3e, 0x45f8, 0x4cf8, [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf]),
    GUID::from_values(0xf581cfb8, 0x88d6, 0x4381, [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab]),
    GUID::from_values(0x36850110, 0x3a07, 0x441f, [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6]),
    GUID::from_values(0x90a7b826, 0xdf06, 0x4862, [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]),
    GUID::from_values(0x21c6e6b4, 0x297a, 0x4cba, [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3]),
    GUID::from_values(0x8e75c279, 0x6299, 0x4ab6, [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5]),
    GUID::from_values(0x84848c12, 0x6f71, 0x4c13, [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74]),
];

// NV_ENC_CAPS
pub const NV_ENC_CAPS_NUM_MAX_BFRAMES: u32 = 0;
pub const NV_ENC_CAPS_WIDTH_MAX: u32 = 16;
pub const NV_ENC_CAPS_HEIGHT_MAX: u32 = 17;
pub const NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE: u32 = 20;
pub const NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION: u32 = 28;
pub const NV_ENC_CAPS_SUPPORT_10BIT_ENCODE: u32 = 39;
pub const NV_ENC_CAPS_SUPPORT_BFRAME_REF_MODE: u32 = 43;

// NV_ENC_BUFFER_FORMAT
pub const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
pub const NV_ENC_BUFFER_FORMAT_YUV420_10BIT: u32 = 0x0001_0000;

// NV_ENC_PIC_FLAGS
pub const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
pub const NV_ENC_PIC_FLAG_EOS: u32 = 0x8;

// NV_ENC_PIC_TYPE
pub const NV_ENC_PIC_TYPE_P: u32 = 0x0;
pub const NV_ENC_PIC_TYPE_B: u32 = 0x1;
pub const NV_ENC_PIC_TYPE_I: u32 = 0x2;
pub const NV_ENC_PIC_TYPE_IDR: u32 = 0x3;

pub const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x1;
pub const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
pub const NV_ENC_MULTI_PASS_DISABLED: u32 = 0x0;
pub const NV_ENC_TWO_PASS_QUARTER_RESOLUTION: u32 = 0x1;
pub const NV_ENC_TWO_PASS_FULL_RESOLUTION: u32 = 0x2;
pub const NV_ENC_TUNING_INFO_HIGH_QUALITY: u32 = 1;
pub const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0x0;
pub const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0x0;
/// NV_ENC_BUFFER_USAGE: a registered resource that holds pictures to encode.
pub const NV_ENC_INPUT_IMAGE: u32 = 0x0;
pub const NV_ENC_MEMORY_HEAP_AUTOSELECT: u32 = 0;
pub const NV_ENC_BFRAME_REF_MODE_DISABLED: u32 = 0x0;
pub const NV_ENC_BFRAME_REF_MODE_MIDDLE: u32 = 0x2;
pub const NV_ENC_TIER_HEVC_HIGH: u32 = 1;

pub const NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED: u32 = 5;
pub const NV_ENC_VUI_COLOR_PRIMARIES_BT709: u32 = 1;
pub const NV_ENC_VUI_COLOR_PRIMARIES_BT2020: u32 = 9;
pub const NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709: u32 = 1;
pub const NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084: u32 = 16;
pub const NV_ENC_VUI_MATRIX_COEFFS_BT709: u32 = 1;
pub const NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL: u32 = 9;

// Bits of NV_ENC_RC_PARAMS::flags (enableMinQP … enableExtLookahead).
pub const RC_ENABLE_AQ: u32 = 1 << 3;
pub const RC_ENABLE_LOOKAHEAD: u32 = 1 << 5;
pub const RC_ENABLE_TEMPORAL_AQ: u32 = 1 << 8;
pub const RC_AQ_STRENGTH_SHIFT: u32 = 12;
pub const RC_AQ_STRENGTH_MASK: u32 = 0xF << RC_AQ_STRENGTH_SHIFT;

// Bits of NV_ENC_CONFIG_HEVC::flags (useConstrainedIntraPred … outputTimeCodeSEI).
pub const HEVC_OUTPUT_AUD: u32 = 1 << 4;
pub const HEVC_DISABLE_SPSPPS: u32 = 1 << 6;
pub const HEVC_REPEAT_SPSPPS: u32 = 1 << 7;
pub const HEVC_CHROMA_FORMAT_SHIFT: u32 = 9;
pub const HEVC_CHROMA_FORMAT_MASK: u32 = 0x3 << HEVC_CHROMA_FORMAT_SHIFT;
pub const HEVC_BIT_DEPTH_MINUS8_SHIFT: u32 = 11;
pub const HEVC_BIT_DEPTH_MINUS8_MASK: u32 = 0x7 << HEVC_BIT_DEPTH_MINUS8_SHIFT;

// Bits of NV_ENC_RECONFIGURE_PARAMS::flags.
pub const RECONFIGURE_RESET_ENCODER: u32 = 1 << 0;
pub const RECONFIGURE_FORCE_IDR: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CAPS_PARAM {
    pub version: u32,
    pub capsToQuery: u32,
    pub reserved: [u32; 62],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CREATE_INPUT_BUFFER {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memoryHeap: u32,
    pub bufferFmt: u32,
    pub reserved: u32,
    pub inputBuffer: *mut c_void,
    pub pSysMemBuffer: *mut c_void,
    pub reserved1: [u32; 57],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CREATE_BITSTREAM_BUFFER {
    pub version: u32,
    pub size: u32,
    pub memoryHeap: u32,
    pub reserved: u32,
    pub bitstreamBuffer: *mut c_void,
    pub bitstreamBufferPtr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_QP {
    pub qpInterP: u32,
    pub qpInterB: u32,
    pub qpIntra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RC_PARAMS {
    pub version: u32,
    pub rateControlMode: u32,
    pub constQP: NV_ENC_QP,
    pub averageBitRate: u32,
    pub maxBitRate: u32,
    pub vbvBufferSize: u32,
    pub vbvInitialDelay: u32,
    /// The bitfields enableMinQP … enableExtLookahead (see the `RC_*` bits).
    pub flags: u32,
    pub minQP: NV_ENC_QP,
    pub maxQP: NV_ENC_QP,
    pub initialRCQP: NV_ENC_QP,
    pub temporallayerIdxMask: u32,
    pub temporalLayerQP: [u8; 8],
    pub targetQuality: u8,
    pub targetQualityLSB: u8,
    pub lookaheadDepth: u16,
    pub lowDelayKeyFrameScale: u8,
    pub yDcQPIndexOffset: i8,
    pub uDcQPIndexOffset: i8,
    pub vDcQPIndexOffset: i8,
    pub qpMapMode: u32,
    pub multiPass: u32,
    pub alphaLayerBitrateRatio: u32,
    pub cbQPIndexOffset: i8,
    pub crQPIndexOffset: i8,
    pub reserved2: u16,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_HEVC_VUI_PARAMETERS {
    pub overscanInfoPresentFlag: u32,
    pub overscanInfo: u32,
    pub videoSignalTypePresentFlag: u32,
    pub videoFormat: u32,
    pub videoFullRangeFlag: u32,
    pub colourDescriptionPresentFlag: u32,
    pub colourPrimaries: u32,
    pub transferCharacteristics: u32,
    pub colourMatrix: u32,
    pub chromaSampleLocationFlag: u32,
    pub chromaSampleLocationTop: u32,
    pub chromaSampleLocationBot: u32,
    pub bitstreamRestrictionFlag: u32,
    pub timingInfoPresentFlag: u32,
    pub numUnitInTicks: u32,
    pub timeScale: u32,
    pub reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG_HEVC {
    pub level: u32,
    pub tier: u32,
    pub minCUSize: u32,
    pub maxCUSize: u32,
    /// The bitfields useConstrainedIntraPred … outputTimeCodeSEI (see the `HEVC_*` bits).
    pub flags: u32,
    pub idrPeriod: u32,
    pub intraRefreshPeriod: u32,
    pub intraRefreshCnt: u32,
    pub maxNumRefFramesInDPB: u32,
    pub ltrNumFrames: u32,
    pub vpsId: u32,
    pub spsId: u32,
    pub ppsId: u32,
    pub sliceMode: u32,
    pub sliceModeData: u32,
    pub maxTemporalLayersMinus1: u32,
    pub hevcVUIParameters: NV_ENC_CONFIG_HEVC_VUI_PARAMETERS,
    pub ltrTrustMode: u32,
    pub useBFramesAsRef: u32,
    pub numRefL0: u32,
    pub numRefL1: u32,
    pub reserved1: [u32; 214],
    pub reserved2: [*mut c_void; 64],
}

/// The codec union: as large as its largest member (the H.264 configuration), which is never used here.
#[repr(C)]
#[derive(Clone, Copy)]
pub union NV_ENC_CODEC_CONFIG {
    pub hevcConfig: NV_ENC_CONFIG_HEVC,
    pub size: [u64; 224],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_CONFIG {
    pub version: u32,
    pub profileGUID: GUID,
    pub gopLength: u32,
    pub frameIntervalP: i32,
    pub monoChromeEncoding: u32,
    pub frameFieldMode: u32,
    pub mvPrecision: u32,
    pub rcParams: NV_ENC_RC_PARAMS,
    pub encodeCodecConfig: NV_ENC_CODEC_CONFIG,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_INITIALIZE_PARAMS {
    pub version: u32,
    pub encodeGUID: GUID,
    pub presetGUID: GUID,
    pub encodeWidth: u32,
    pub encodeHeight: u32,
    pub darWidth: u32,
    pub darHeight: u32,
    pub frameRateNum: u32,
    pub frameRateDen: u32,
    pub enableEncodeAsync: u32,
    pub enablePTD: u32,
    /// The bitfields reportSliceOffsets … enableOutputStats.
    pub flags: u32,
    pub privDataSize: u32,
    pub privData: *mut c_void,
    pub encodeConfig: *mut NV_ENC_CONFIG,
    pub maxEncodeWidth: u32,
    pub maxEncodeHeight: u32,
    /// Two NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE (16 bytes each).
    pub maxMEHintCountsPerBlock: [[u32; 4]; 2],
    pub tuningInfo: u32,
    pub bufferFormat: u32,
    pub numStateBuffers: u32,
    pub outputStatsLevel: u32,
    pub reserved: [u32; 285],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_RECONFIGURE_PARAMS {
    pub version: u32,
    pub reInitEncodeParams: NV_ENC_INITIALIZE_PARAMS,
    /// resetEncoder (bit 0), forceIDR (bit 1).
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PRESET_CONFIG {
    pub version: u32,
    pub presetCfg: NV_ENC_CONFIG,
    pub reserved1: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_PIC_PARAMS {
    pub version: u32,
    pub inputWidth: u32,
    pub inputHeight: u32,
    pub inputPitch: u32,
    pub encodePicFlags: u32,
    pub frameIdx: u32,
    pub inputTimeStamp: u64,
    pub inputDuration: u64,
    pub inputBuffer: *mut c_void,
    pub outputBitstream: *mut c_void,
    pub completionEvent: *mut c_void,
    pub bufferFmt: u32,
    pub pictureStruct: u32,
    pub pictureType: u32,
    /// NV_ENC_CODEC_PIC_PARAMS (all zero here), then the ME hints … outputReconBuffer, then the reserved arrays.
    pub codecPicParams: [u64; 194],
    pub rest: [u64; 216],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_LOCK_BITSTREAM {
    pub version: u32,
    /// doNotWait (bit 0), ltrFrame, getRCStats.
    pub flags: u32,
    pub outputBitstream: *mut c_void,
    pub sliceOffsets: *mut u32,
    pub frameIdx: u32,
    pub hwEncodeStatus: u32,
    pub numSlices: u32,
    pub bitstreamSizeInBytes: u32,
    pub outputTimeStamp: u64,
    pub outputDuration: u64,
    pub bitstreamBufferPtr: *mut c_void,
    pub pictureType: u32,
    pub pictureStruct: u32,
    pub frameAvgQP: u32,
    pub frameSatd: u32,
    /// ltrFrameIdx … frameIdxDisplay, the reserved arrays and reservedInternal.
    pub rest: [u32; 368],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_LOCK_INPUT_BUFFER {
    pub version: u32,
    /// doNotWait (bit 0).
    pub flags: u32,
    pub inputBuffer: *mut c_void,
    pub bufferDataPtr: *mut c_void,
    pub pitch: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_REGISTER_RESOURCE {
    pub version: u32,
    pub resourceType: u32,
    pub width: u32,
    pub height: u32,
    /// 0 for Direct3D resources.
    pub pitch: u32,
    pub subResourceIndex: u32,
    pub resourceToRegister: *mut c_void,
    pub registeredResource: *mut c_void,
    pub bufferFormat: u32,
    pub bufferUsage: u32,
    /// Direct3D 12 only.
    pub pInputFencePoint: *mut c_void,
    pub chromaOffset: [u32; 2],
    pub reserved1: [u32; 245],
    pub reserved2: [*mut c_void; 61],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_MAP_INPUT_RESOURCE {
    pub version: u32,
    pub subResourceIndex: u32,
    pub inputResource: *mut c_void,
    pub registeredResource: *mut c_void,
    pub mappedResource: *mut c_void,
    pub mappedBufferFmt: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
    pub version: u32,
    pub deviceType: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub apiVersion: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

pub type Encoder = *mut c_void;
type Fn1<A> = Option<unsafe extern "system" fn(Encoder, A) -> NVENCSTATUS>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NV_ENCODE_API_FUNCTION_LIST {
    pub version: u32,
    pub reserved: u32,
    /// nvEncOpenEncodeSession … nvEncGetEncodeGUIDs, nvEncGetInputFormatCount, nvEncGetInputFormats
    pub unused1: [*mut c_void; 7],
    pub nvEncGetEncodeCaps: Option<unsafe extern "system" fn(Encoder, GUID, *mut NV_ENC_CAPS_PARAM, *mut c_int) -> NVENCSTATUS>,
    /// nvEncGetEncodePresetCount, nvEncGetEncodePresetGUIDs, nvEncGetEncodePresetConfig
    pub unused2: [*mut c_void; 3],
    pub nvEncInitializeEncoder: Fn1<*mut NV_ENC_INITIALIZE_PARAMS>,
    pub nvEncCreateInputBuffer: Fn1<*mut NV_ENC_CREATE_INPUT_BUFFER>,
    pub nvEncDestroyInputBuffer: Fn1<*mut c_void>,
    pub nvEncCreateBitstreamBuffer: Fn1<*mut NV_ENC_CREATE_BITSTREAM_BUFFER>,
    pub nvEncDestroyBitstreamBuffer: Fn1<*mut c_void>,
    pub nvEncEncodePicture: Fn1<*mut NV_ENC_PIC_PARAMS>,
    pub nvEncLockBitstream: Fn1<*mut NV_ENC_LOCK_BITSTREAM>,
    pub nvEncUnlockBitstream: Fn1<*mut c_void>,
    pub nvEncLockInputBuffer: Fn1<*mut NV_ENC_LOCK_INPUT_BUFFER>,
    pub nvEncUnlockInputBuffer: Fn1<*mut c_void>,
    /// nvEncGetEncodeStats, nvEncGetSequenceParams, nvEncRegisterAsyncEvent, nvEncUnregisterAsyncEvent
    pub unused3: [*mut c_void; 4],
    pub nvEncMapInputResource: Fn1<*mut NV_ENC_MAP_INPUT_RESOURCE>,
    pub nvEncUnmapInputResource: Fn1<*mut c_void>,
    pub nvEncDestroyEncoder: Option<unsafe extern "system" fn(Encoder) -> NVENCSTATUS>,
    pub nvEncInvalidateRefFrames: Fn1<u64>,
    pub nvEncOpenEncodeSessionEx: Option<unsafe extern "system" fn(*mut NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut Encoder) -> NVENCSTATUS>,
    pub nvEncRegisterResource: Fn1<*mut NV_ENC_REGISTER_RESOURCE>,
    pub nvEncUnregisterResource: Fn1<*mut c_void>,
    pub nvEncReconfigureEncoder: Fn1<*mut NV_ENC_RECONFIGURE_PARAMS>,
    /// reserved1, nvEncCreateMVBuffer, nvEncDestroyMVBuffer, nvEncRunMotionEstimationOnly
    pub unused5: [*mut c_void; 4],
    pub nvEncGetLastErrorString: Option<unsafe extern "system" fn(Encoder) -> *const c_char>,
    /// nvEncSetIOCudaStreams
    pub unused6: [*mut c_void; 1],
    pub nvEncGetEncodePresetConfigEx: Option<unsafe extern "system" fn(Encoder, GUID, GUID, u32, *mut NV_ENC_PRESET_CONFIG) -> NVENCSTATUS>,
    /// nvEncGetSequenceParamEx, nvEncRestoreEncoderState, nvEncLookaheadPicture, and the reserved array
    pub unused7: [*mut c_void; 3 + 275],
}

pub type NvEncodeAPICreateInstance = unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
pub type NvEncodeAPIGetMaxSupportedVersion = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

/// A zeroed structure: how the header's users start every one of these (all-zero is valid for each).
pub fn zeroed<T: Copy>() -> T {
    // SAFETY: only used for the plain C structures above, made of integers, raw pointers, nullable function
    // pointers and GUIDs, for all of which all-zero bytes are a valid value.
    unsafe { std::mem::zeroed() }
}

// The layout as MSVC computed it from nvEncodeAPI.h 12.1.
const _: () = {
    assert!(size_of::<GUID>() == 16);
    assert!(size_of::<NV_ENC_CAPS_PARAM>() == 256);
    assert!(offset_of!(NV_ENC_CAPS_PARAM, capsToQuery) == 4);

    assert!(size_of::<NV_ENC_CREATE_INPUT_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, memoryHeap) == 12);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, bufferFmt) == 16);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, inputBuffer) == 24);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, pSysMemBuffer) == 32);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, reserved1) == 40);
    assert!(offset_of!(NV_ENC_CREATE_INPUT_BUFFER, reserved2) == 272);

    assert!(size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>() == 776);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer) == 16);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBufferPtr) == 24);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved1) == 32);
    assert!(offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, reserved2) == 264);

    assert!(size_of::<NV_ENC_QP>() == 12);
    assert!(size_of::<NV_ENC_RC_PARAMS>() == 128);
    assert!(offset_of!(NV_ENC_RC_PARAMS, rateControlMode) == 4);
    assert!(offset_of!(NV_ENC_RC_PARAMS, constQP) == 8);
    assert!(offset_of!(NV_ENC_RC_PARAMS, averageBitRate) == 20);
    assert!(offset_of!(NV_ENC_RC_PARAMS, maxBitRate) == 24);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vbvBufferSize) == 28);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vbvInitialDelay) == 32);
    assert!(offset_of!(NV_ENC_RC_PARAMS, flags) == 36);
    assert!(offset_of!(NV_ENC_RC_PARAMS, minQP) == 40);
    assert!(offset_of!(NV_ENC_RC_PARAMS, maxQP) == 52);
    assert!(offset_of!(NV_ENC_RC_PARAMS, initialRCQP) == 64);
    assert!(offset_of!(NV_ENC_RC_PARAMS, temporallayerIdxMask) == 76);
    assert!(offset_of!(NV_ENC_RC_PARAMS, temporalLayerQP) == 80);
    assert!(offset_of!(NV_ENC_RC_PARAMS, targetQuality) == 88);
    assert!(offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth) == 90);
    assert!(offset_of!(NV_ENC_RC_PARAMS, lowDelayKeyFrameScale) == 92);
    assert!(offset_of!(NV_ENC_RC_PARAMS, vDcQPIndexOffset) == 95);
    assert!(offset_of!(NV_ENC_RC_PARAMS, qpMapMode) == 96);
    assert!(offset_of!(NV_ENC_RC_PARAMS, multiPass) == 100);
    assert!(offset_of!(NV_ENC_RC_PARAMS, alphaLayerBitrateRatio) == 104);
    assert!(offset_of!(NV_ENC_RC_PARAMS, cbQPIndexOffset) == 108);
    assert!(offset_of!(NV_ENC_RC_PARAMS, crQPIndexOffset) == 109);
    assert!(offset_of!(NV_ENC_RC_PARAMS, reserved2) == 110);
    assert!(offset_of!(NV_ENC_RC_PARAMS, reserved) == 112);
    // bit positions: enableAQ 0x8, enableLookahead 0x20, enableTemporalAQ 0x100, aqStrength 0xf000
    assert!(RC_ENABLE_AQ == 0x8 && RC_ENABLE_LOOKAHEAD == 0x20 && RC_ENABLE_TEMPORAL_AQ == 0x100);
    assert!(RC_AQ_STRENGTH_MASK == 0xf000);

    assert!(size_of::<NV_ENC_CONFIG_HEVC_VUI_PARAMETERS>() == 112);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, videoSignalTypePresentFlag) == 8);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, videoFormat) == 12);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, videoFullRangeFlag) == 16);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, colourDescriptionPresentFlag) == 20);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, colourPrimaries) == 24);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, transferCharacteristics) == 28);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, colourMatrix) == 32);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, chromaSampleLocationFlag) == 36);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, chromaSampleLocationTop) == 40);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, chromaSampleLocationBot) == 44);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, timeScale) == 60);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC_VUI_PARAMETERS, reserved) == 64);

    assert!(size_of::<NV_ENC_CONFIG_HEVC>() == 1560);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, tier) == 4);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, flags) == 16);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, idrPeriod) == 20);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, maxNumRefFramesInDPB) == 32);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, sliceMode) == 52);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, sliceModeData) == 56);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, maxTemporalLayersMinus1) == 60);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, hevcVUIParameters) == 64);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, ltrTrustMode) == 176);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, useBFramesAsRef) == 180);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, numRefL0) == 184);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, numRefL1) == 188);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, reserved1) == 192);
    assert!(offset_of!(NV_ENC_CONFIG_HEVC, reserved2) == 1048);
    // bit positions: outputAUD 0x10, disableSPSPPS 0x40, repeatSPSPPS 0x80, chromaFormatIDC 0x600,
    // pixelBitDepthMinus8 0x3800
    assert!(HEVC_OUTPUT_AUD == 0x10 && HEVC_DISABLE_SPSPPS == 0x40 && HEVC_REPEAT_SPSPPS == 0x80);
    assert!(HEVC_CHROMA_FORMAT_MASK == 0x600 && HEVC_BIT_DEPTH_MINUS8_MASK == 0x3800);

    assert!(size_of::<NV_ENC_CODEC_CONFIG>() == 1792);
    assert!(size_of::<NV_ENC_CONFIG>() == 3584);
    assert!(offset_of!(NV_ENC_CONFIG, profileGUID) == 4);
    assert!(offset_of!(NV_ENC_CONFIG, gopLength) == 20);
    assert!(offset_of!(NV_ENC_CONFIG, frameIntervalP) == 24);
    assert!(offset_of!(NV_ENC_CONFIG, frameFieldMode) == 32);
    assert!(offset_of!(NV_ENC_CONFIG, mvPrecision) == 36);
    assert!(offset_of!(NV_ENC_CONFIG, rcParams) == 40);
    assert!(offset_of!(NV_ENC_CONFIG, encodeCodecConfig) == 168);
    assert!(offset_of!(NV_ENC_CONFIG, reserved) == 1960);
    assert!(offset_of!(NV_ENC_CONFIG, reserved2) == 3072);

    assert!(size_of::<NV_ENC_INITIALIZE_PARAMS>() == 1808);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeGUID) == 4);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, presetGUID) == 20);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeWidth) == 36);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, darWidth) == 44);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateNum) == 52);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateDen) == 56);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync) == 60);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, enablePTD) == 64);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, flags) == 68);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privDataSize) == 72);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, privData) == 80);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig) == 88);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth) == 96);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeHeight) == 100);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, maxMEHintCountsPerBlock) == 104);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo) == 136);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, bufferFormat) == 140);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, numStateBuffers) == 144);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, outputStatsLevel) == 148);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved) == 152);
    assert!(offset_of!(NV_ENC_INITIALIZE_PARAMS, reserved2) == 1296);

    assert!(size_of::<NV_ENC_RECONFIGURE_PARAMS>() == 1824);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams) == 8);
    assert!(offset_of!(NV_ENC_RECONFIGURE_PARAMS, flags) == 1816);
    assert!(RECONFIGURE_RESET_ENCODER == 0x1 && RECONFIGURE_FORCE_IDR == 0x2);

    assert!(size_of::<NV_ENC_PRESET_CONFIG>() == 5128);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, presetCfg) == 8);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, reserved1) == 3592);
    assert!(offset_of!(NV_ENC_PRESET_CONFIG, reserved2) == 4616);

    assert!(size_of::<NV_ENC_PIC_PARAMS>() == 3360);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputPitch) == 12);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, encodePicFlags) == 16);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, frameIdx) == 20);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp) == 24);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputDuration) == 32);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, inputBuffer) == 40);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, outputBitstream) == 48);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, completionEvent) == 56);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, bufferFmt) == 64);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, pictureStruct) == 68);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, pictureType) == 72);
    assert!(offset_of!(NV_ENC_PIC_PARAMS, codecPicParams) == 80);
    // NV_ENC_CODEC_PIC_PARAMS is 1552 bytes; meHintCountsPerBlock follows at 1632
    assert!(offset_of!(NV_ENC_PIC_PARAMS, rest) == 1632);

    assert!(size_of::<NV_ENC_LOCK_BITSTREAM>() == 1552);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, flags) == 4);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream) == 8);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, sliceOffsets) == 16);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameIdx) == 24);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, hwEncodeStatus) == 28);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, numSlices) == 32);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes) == 36);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp) == 40);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, outputDuration) == 48);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr) == 56);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType) == 64);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, pictureStruct) == 68);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameAvgQP) == 72);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, frameSatd) == 76);
    assert!(offset_of!(NV_ENC_LOCK_BITSTREAM, rest) == 80);

    assert!(size_of::<NV_ENC_LOCK_INPUT_BUFFER>() == 1544);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, inputBuffer) == 8);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, bufferDataPtr) == 16);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, pitch) == 24);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, reserved1) == 28);
    assert!(offset_of!(NV_ENC_LOCK_INPUT_BUFFER, reserved2) == 1032);

    assert!(size_of::<NV_ENC_REGISTER_RESOURCE>() == 1536);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, resourceType) == 4);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, width) == 8);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, height) == 12);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, pitch) == 16);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, subResourceIndex) == 20);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, resourceToRegister) == 24);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, registeredResource) == 32);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, bufferFormat) == 40);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, bufferUsage) == 44);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, pInputFencePoint) == 48);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, chromaOffset) == 56);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, reserved1) == 64);
    assert!(offset_of!(NV_ENC_REGISTER_RESOURCE, reserved2) == 1048);

    assert!(size_of::<NV_ENC_MAP_INPUT_RESOURCE>() == 1544);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, subResourceIndex) == 4);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, inputResource) == 8);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, registeredResource) == 16);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedResource) == 24);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt) == 32);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, reserved1) == 36);
    assert!(offset_of!(NV_ENC_MAP_INPUT_RESOURCE, reserved2) == 1040);

    assert!(size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>() == 1552);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, deviceType) == 4);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device) == 8);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved) == 16);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion) == 24);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved1) == 28);
    assert!(offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, reserved2) == 1040);

    assert!(size_of::<NV_ENCODE_API_FUNCTION_LIST>() == 2552);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps) == 64);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder) == 96);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateInputBuffer) == 104);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyInputBuffer) == 112);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateBitstreamBuffer) == 120);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyBitstreamBuffer) == 128);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture) == 136);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockBitstream) == 144);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockBitstream) == 152);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockInputBuffer) == 160);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer) == 168);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, unused3) == 176);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncMapInputResource) == 208);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnmapInputResource) == 216);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder) == 224);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInvalidateRefFrames) == 232);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx) == 240);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterResource) == 248);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterResource) == 256);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder) == 264);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString) == 304);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx) == 320);
    assert!(offset_of!(NV_ENCODE_API_FUNCTION_LIST, unused7) == 328);

    // the version words, as the header's macros make them
    assert!(NVENCAPI_VERSION == 16777228);
    assert!(NV_ENC_CAPS_PARAM_VER == 1895890956);
    assert!(NV_ENC_CREATE_INPUT_BUFFER_VER == 1895890956);
    assert!(NV_ENC_CREATE_BITSTREAM_BUFFER_VER == 1895890956);
    assert!(NV_ENC_CONFIG_VER == 4043833356);
    assert!(NV_ENC_INITIALIZE_PARAMS_VER == 4043702284);
    assert!(NV_ENC_RECONFIGURE_PARAMS_VER == 4043374604);
    assert!(NV_ENC_PRESET_CONFIG_VER == 4043571212);
    assert!(NV_ENC_PIC_PARAMS_VER == 4043702284);
    assert!(NV_ENC_LOCK_BITSTREAM_VER == 4043374604);
    assert!(NV_ENC_LOCK_INPUT_BUFFER_VER == 1895890956);
    assert!(NV_ENC_MAP_INPUT_RESOURCE_VER == 1896087564);
    assert!(NV_ENC_REGISTER_RESOURCE_VER == 1896087564);
    assert!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER == 1895890956);
    assert!(NV_ENCODE_API_FUNCTION_LIST_VER == 1895956492);
};
