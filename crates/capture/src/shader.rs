//! HLSL source of the desktop -> P010 / NV12 conversion compute shader (design.md section 2.3), compiled at
//! runtime with d3dcompiler_47 (cs_5_0). Two entry points share the helpers:
//!   main_p010 - one thread per 2x2 block (8x8 groups): 4 luma + 1 averaged chroma pair, 4-byte stores.
//!   main_nv12 - one thread per 4 Y bytes / per 2 chroma pairs (linear addressing): RWByteAddressBuffer
//!               stores are 4-byte aligned, and an NV12 row is W bytes, so 2x2 blocks cannot be written
//!               directly when W % 4 == 2 (e.g. 1366-wide displays). Colour maths is cheap for SDR, so the
//!               2x recompute of the chroma path does not matter.
//! Constant buffer layout must match [`ShaderParams`] (64 bytes).

pub const FLAG_CURSOR_VISIBLE: u32 = 1;
/// source texture is FP16 scRGB (HDR desktop); else BGRA8 sRGB
pub const FLAG_SOURCE_LINEAR: u32 = 2;
/// no scaling / rotation: use Load() instead of the sampler
pub const FLAG_EXACT: u32 = 4;
/// informational only (logging)
pub const FLAG_SOURCE_IS_HDR_FORCED_SDR: u32 = 8;

pub const ENTRY_P010: &str = "main_p010";
pub const ENTRY_NV12: &str = "main_nv12";
pub const PROFILE: &str = "cs_5_0";

/// Threads per group in X and Y for main_p010 (each thread = one 2x2 block).
pub const P010_GROUP_SIZE: u32 = 8;
/// Threads per group for main_nv12 (1-D, folded into a 2-D dispatch of [`NV12_GROUPS_X`] columns).
pub const NV12_GROUP_SIZE: u32 = 64;
pub const NV12_GROUPS_X: u32 = 1024;

/// The shader source. ASCII only, and always handed to the compiler as bytes with their exact length:
/// the C# engine once passed it as a string, which SharpDX converted to the system ANSI code page while still
/// passing the UTF-16 length, so every character that became two bytes cut one byte off the end. On Chinese
/// Windows the shader lost its closing brace (0.2.2). A test keeps the source ASCII.
pub const SOURCE: &str = include_str!("hdrcast_convert.hlsl");

/// The source as the compiler must get it (see [`SOURCE`]).
pub fn source_bytes() -> &'static [u8] {
    SOURCE.as_bytes()
}

/// Constant buffer of the conversion shader - must match "cbuffer Params" in the HLSL (64 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShaderParams {
    /// duplicated texture size (unrotated framebuffer)
    pub src_width: u32,
    pub src_height: u32,
    /// desktop size as seen by the user (rotated)
    pub desk_width: u32,
    pub desk_height: u32,
    /// output frame size (even)
    pub dst_width: u32,
    pub dst_height: u32,
    /// 0 identity, 1 = 90, 2 = 180, 3 = 270 (DXGI_MODE_ROTATION - 1)
    pub rotation: u32,
    /// `FLAG_*` bits
    pub flags: u32,
    /// nits represented by scRGB 1.0 (default 80)
    pub sdr_white: f32,
    /// cursor top-left (hotspot already subtracted), desktop pixels
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub cursor_w: u32,
    pub cursor_h: u32,
    pub pad0: f32,
    pub pad1: f32,
    pub pad2: f32,
}

impl ShaderParams {
    pub const SIZE_IN_BYTES: usize = 64;
}

// The constant buffer layout is duplicated in HLSL; a mismatch would silently corrupt every frame.
const _: () = assert!(std::mem::size_of::<ShaderParams>() == ShaderParams::SIZE_IN_BYTES);

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.2.2 on Chinese Windows: "hdrcast_convert.hlsl(187,6): error X3000: syntax error: unexpected end of
    /// file". Comments held a section sign and two em dashes, two bytes each in GBK and Big5, so the last 3
    /// bytes of the shader (its closing brace) never reached the compiler. Japanese Windows only lost the
    /// final newline.
    #[test]
    fn the_shader_source_is_plain_ascii() {
        for (offset, c) in SOURCE.char_indices() {
            assert!(
                c.is_ascii(),
                "non-ASCII U+{:04X} at offset {offset}",
                c as u32
            );
        }
        assert_eq!(SOURCE.chars().count(), source_bytes().len());
    }

    #[test]
    fn both_entry_points_are_in_the_source() {
        assert!(SOURCE.contains("void main_p010("));
        assert!(SOURCE.contains("void main_nv12("));
        assert!(SOURCE.trim_end().ends_with('}'));
    }
}
