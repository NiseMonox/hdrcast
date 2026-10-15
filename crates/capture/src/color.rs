//! Rust port of the compute-shader colour maths (design.md section 2.3). It is the reference the unit tests
//! check and what `capture --selftest` compares the GPU output against. Keep it identical to the HLSL in
//! [`crate::shader`].

use crate::args::CaptureFormat;

/// BT.709 -> BT.2020 primaries (row-major; each row sums to 1 so white is preserved).
pub const M709_TO_2020: [[f64; 3]; 3] = [
    [0.6274039, 0.3292830, 0.0433131],
    [0.0690973, 0.9195404, 0.0113623],
    [0.0163914, 0.0880133, 0.8955953],
];

// BT.2020 non-constant-luminance luma coefficients
pub const KR_2020: f64 = 0.2627;
pub const KG_2020: f64 = 0.6780;
pub const KB_2020: f64 = 0.0593;
// BT.709 luma coefficients
pub const KR_709: f64 = 0.2126;
pub const KG_709: f64 = 0.7152;
pub const KB_709: f64 = 0.0722;

const PQ_M1: f64 = 0.1593017578125;
const PQ_M2: f64 = 78.84375;
const PQ_C1: f64 = 0.8359375;
const PQ_C2: f64 = 18.8515625;
const PQ_C3: f64 = 18.6875;

/// SMPTE ST 2084 inverse EOTF. Input in units of 10000 nit (1.0 = 10000 nit); output in [0,1].
pub fn pq_oetf(x: f64) -> f64 {
    let p = x.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// Inverse of [`pq_oetf`]: PQ signal [0,1] -> luminance in nit.
pub fn pq_eotf_nits(e: f64) -> f64 {
    let e = e.clamp(0.0, 1.0);
    let p = e.powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * p;
    (num / den).powf(1.0 / PQ_M1) * 10000.0
}

pub fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(c: f64) -> f64 {
    let c = saturate(c);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

pub fn saturate(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// Rounds like HLSL `round()` for non-negative values (half away from zero).
pub fn round_to_int(v: f64) -> i32 {
    v.round() as i32
}

/// scRGB linear (BT.709 primaries, 1.0 = `sdr_white` nit) -> PQ-encoded BT.2020 RGB in [0,1]
/// (the "E" vector of design.md section 2.3; negative out-of-gamut values are clipped).
pub fn scrgb_to_pq2020(r: f64, g: f64, b: f64, sdr_white: f64) -> (f64, f64, f64) {
    let m = &M709_TO_2020;
    let lr = (m[0][0] * r + m[0][1] * g + m[0][2] * b).max(0.0);
    let lg = (m[1][0] * r + m[1][1] * g + m[1][2] * b).max(0.0);
    let lb = (m[2][0] * r + m[2][1] * g + m[2][2] * b).max(0.0);
    (
        saturate(pq_oetf(lr * sdr_white / 10000.0)),
        saturate(pq_oetf(lg * sdr_white / 10000.0)),
        saturate(pq_oetf(lb * sdr_white / 10000.0)),
    )
}

/// Full HDR10 pixel conversion: scRGB -> 10-bit limited-range Y'CbCr (NCL, BT.2020) as stored in P010 before
/// the `<< 6` shift. Returns `(y10, cb10, cr10)`.
pub fn scrgb_to_yuv10(r: f64, g: f64, b: f64, sdr_white: f64) -> (i32, i32, i32) {
    let (er, eg, eb) = scrgb_to_pq2020(r, g, b, sdr_white);
    let y = KR_2020 * er + KG_2020 * eg + KB_2020 * eb;
    let cb = (eb - y) / 1.8814;
    let cr = (er - y) / 1.4746;
    (
        round_to_int(64.0 + 876.0 * y),
        round_to_int(512.0 + 896.0 * cb),
        round_to_int(512.0 + 896.0 * cr),
    )
}

/// Luma-only helper (grey values): scRGB value -> Y10.
pub fn scrgb_grey_to_y10(v: f64, sdr_white: f64) -> i32 {
    scrgb_to_yuv10(v, v, v, sdr_white).0
}

/// The 16-bit sample as stored in a p010le frame for a 10-bit code (`value << 6`).
pub fn to_p010_sample(code10: i32) -> u16 {
    ((code10 & 0x3FF) << 6) as u16
}

pub fn from_p010_sample(sample: u16) -> i32 {
    i32::from(sample >> 6)
}

/// SDR pixel conversion: sRGB-encoded RGB (values in [0,1], used directly as BT.709 gamma) -> 8-bit
/// limited-range Y'CbCr for NV12. Returns `(y8, cb8, cr8)`.
pub fn srgb_to_yuv8(r: f64, g: f64, b: f64) -> (i32, i32, i32) {
    let (r, g, b) = (saturate(r), saturate(g), saturate(b));
    let y = KR_709 * r + KG_709 * g + KB_709 * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    (
        round_to_int(16.0 + 219.0 * y),
        round_to_int(128.0 + 224.0 * cb),
        round_to_int(128.0 + 224.0 * cr),
    )
}

/// Byte size of one raw frame for the given output format (W and H must be even).
///
/// # Panics
/// When `format` is still [`CaptureFormat::Auto`].
pub fn frame_bytes(format: CaptureFormat, width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    match format {
        CaptureFormat::P010 => w * h * 2 + w * (h / 2) * 2,
        CaptureFormat::Nv12 => w * h + w * (h / 2),
        CaptureFormat::Auto => panic!("format must be resolved to p010 or nv12"),
    }
}

/// Byte offset of the interleaved UV plane inside a raw frame.
pub fn uv_plane_offset(format: CaptureFormat, width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    if format == CaptureFormat::P010 {
        w * h * 2
    } else {
        w * h
    }
}

/// Fills a frame buffer with "black" (Y = 64/16, Cb = Cr = 512/128) so the encoder gets a valid picture
/// before the first capture.
///
/// # Panics
/// When `frame` is smaller than one frame, or `format` is still [`CaptureFormat::Auto`].
pub fn fill_black(frame: &mut [u8], format: CaptureFormat, width: u32, height: u32) {
    let uv = uv_plane_offset(format, width, height);
    let total = frame_bytes(format, width, height);
    assert!(frame.len() >= total, "frame buffer too small");
    let (luma, chroma) = frame[..total].split_at_mut(uv);
    if format == CaptureFormat::P010 {
        let y = to_p010_sample(64).to_le_bytes();
        let c = to_p010_sample(512).to_le_bytes();
        for sample in luma.chunks_exact_mut(2) {
            sample.copy_from_slice(&y);
        }
        for sample in chroma.chunks_exact_mut(2) {
            sample.copy_from_slice(&c);
        }
    } else {
        luma.fill(16);
        chroma.fill(128);
    }
}
