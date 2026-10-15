//! `capture --selftest`: runs the conversion shader on a synthetic scRGB gradient and compares the GPU result
//! with the reference implementation ([`crate::color`]). This verifies the HDR maths on the user's own GPU
//! without touching the desktop, so a colour problem can be located quickly.
//! Reference values (ST.2084 at sdr-white 80 nit, limited-range 10 bit): scRGB 1.0 -> Y10 490,
//! 2.5 -> 571, 125 -> 940 (clipped at the limited-range maximum). ffmpeg's zscale with npl=80 lands within
//! 3 code values of these; the GPU must match the reference, which is what this test checks.

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

use super::converter::{create_shader_texture, GpuConverter};
use super::outputs::adapter_name;
use super::util::error_text;
use crate::args::CaptureFormat;
use crate::color;
use crate::engine::exit_codes;
use crate::log;
use crate::numfmt::trimmed;
use crate::shader::{self, ShaderParams};

const TEST_VALUES: [f32; 6] = [0.0, 0.5, 1.0, 2.5, 12.5, 125.0];

/// One 16 px column per test value.
const WIDTH: u32 = 16 * TEST_VALUES.len() as u32;
const HEIGHT: u32 = 16;

/// What the shader made of the test pattern.
pub(crate) struct SelfTestResult {
    pub device: String,
    /// (scRGB input, Y10 from the GPU, Y10 from the reference) per test value.
    pub luma: Vec<(f32, i32, i32)>,
    /// Chroma of the grey pattern (expected 512 / 512).
    pub cb: i32,
    pub cr: i32,
}

impl SelfTestResult {
    pub(crate) fn luma_ok(gpu: i32, reference: i32) -> bool {
        (gpu - reference).abs() <= 2
    }

    pub(crate) fn chroma_ok(&self) -> bool {
        (self.cb - 512).abs() <= 2 && (self.cr - 512).abs() <= 2
    }

    pub(crate) fn failures(&self) -> usize {
        self.luma
            .iter()
            .filter(|(_, gpu, reference)| !Self::luma_ok(*gpu, *reference))
            .count()
            + usize::from(!self.chroma_ok())
    }
}

pub(crate) fn run(sdr_white: f32) -> i32 {
    let result = match measure(sdr_white) {
        Ok(result) => result,
        Err(message) => {
            log::stderr_text(&format!("selftest: {message}\n"));
            return exit_codes::DEVICE_ERROR;
        }
    };

    let mut out = format!("device: {}\n", result.device);
    out.push_str(&format!(
        "scRGB -> HDR10 (PQ, BT.2020, limited range 10 bit), sdr-white = {} nit\n",
        trimmed(f64::from(sdr_white), 1)
    ));
    out.push_str("  input      GPU Y10   reference   nits      status\n");
    for &(value, gpu, reference) in &result.luma {
        let nits = color::pq_eotf_nits((f64::from(gpu - 64) / 876.0).max(0.0));
        out.push_str(&format!(
            "  {:<10} {:>7}   {:>9}   {:>8}  {}\n",
            trimmed(f64::from(value), 3),
            gpu,
            reference,
            trimmed(nits, 1),
            if SelfTestResult::luma_ok(gpu, reference) {
                "ok"
            } else {
                "MISMATCH"
            }
        ));
    }
    // The UV plane must be neutral for grey input (Cb = Cr = 512).
    out.push_str(&format!(
        "  chroma of grey: Cb={} Cr={}{}\n",
        result.cb,
        result.cr,
        if result.chroma_ok() {
            " ok"
        } else {
            " MISMATCH (expected 512/512)"
        }
    ));
    let failures = result.failures();
    if failures == 0 {
        out.push_str(
            "selftest: PASS \u{2014} the GPU shader matches the reference implementation\n",
        );
    } else {
        out.push_str(&format!(
            "selftest: FAIL \u{2014} {failures} mismatch(es)\n"
        ));
    }
    log::stdout_text(&out);
    if failures == 0 {
        exit_codes::OK
    } else {
        exit_codes::DEVICE_ERROR
    }
}

/// Runs the shader on the test pattern and reads the interesting samples back.
pub(crate) fn measure(sdr_white: f32) -> Result<SelfTestResult, String> {
    let (device, description) = create_device()?;

    // An R16G16B16A16_FLOAT texture with one constant scRGB column per test value.
    let mut pixels = vec![0u16; (WIDTH * HEIGHT * 4) as usize];
    for (i, pixel) in pixels.chunks_exact_mut(4).enumerate() {
        let x = i as u32 % WIDTH;
        let v = float_to_half(TEST_VALUES[(x / 16).min(TEST_VALUES.len() as u32 - 1) as usize]);
        pixel.copy_from_slice(&[v, v, v, float_to_half(1.0)]);
    }
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: WIDTH * 8,
        SysMemSlicePitch: 0,
    };
    // `pixels` outlives the call that reads `initial`.
    let (source, _view) = create_shader_texture(
        &device,
        WIDTH,
        HEIGHT,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        Some(&initial),
    )?;

    let mut converter = GpuConverter::new(&device, CaptureFormat::P010, WIDTH, HEIGHT)?;
    converter.update_source(&source)?;

    let mut frame = vec![0u8; converter.frame_bytes()];
    let params = ShaderParams {
        src_width: WIDTH,
        src_height: HEIGHT,
        desk_width: WIDTH,
        desk_height: HEIGHT,
        dst_width: WIDTH,
        dst_height: HEIGHT,
        rotation: 0,
        flags: shader::FLAG_SOURCE_LINEAR | shader::FLAG_EXACT,
        sdr_white,
        cursor_w: 1,
        cursor_h: 1,
        ..ShaderParams::default()
    };
    converter.convert(&params, &mut frame)?;

    let sample = |offset: usize| {
        color::from_p010_sample(u16::from_le_bytes([frame[offset], frame[offset + 1]]))
    };
    let luma = TEST_VALUES
        .iter()
        .enumerate()
        .map(|(i, &value)| {
            let (x, y) = (i * 16 + 8, 8);
            let gpu = sample((y * WIDTH as usize + x) * 2);
            (
                value,
                gpu,
                color::scrgb_grey_to_y10(f64::from(value), f64::from(sdr_white)),
            )
        })
        .collect();

    let uv_offset = color::uv_plane_offset(CaptureFormat::P010, WIDTH, HEIGHT);
    let uv_index = uv_offset + (HEIGHT as usize / 2 - 1) * WIDTH as usize * 2 + 16;
    Ok(SelfTestResult {
        device: description,
        luma,
        cb: sample(uv_index),
        cr: sample(uv_index + 2),
    })
}

fn create_device_on(
    adapter: Option<&IDXGIAdapter>,
    driver: D3D_DRIVER_TYPE,
) -> Result<ID3D11Device, String> {
    const LEVELS: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    // SAFETY: plain device creation; the out pointer is a valid local.
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|e| error_text(&e))?;
    device.ok_or_else(|| "no Direct3D 11 device".to_owned())
}

/// The first adapter's device, or WARP when there is no usable hardware.
fn create_device() -> Result<(ID3D11Device, String), String> {
    let hardware = (|| -> Result<Option<(ID3D11Device, String)>, String> {
        // SAFETY: plain factory creation and enumeration.
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| error_text(&e))?;
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(0) }) else {
            return Ok(None);
        };
        let description = adapter_name(&adapter, 0);
        let adapter = adapter.cast::<IDXGIAdapter>().map_err(|e| error_text(&e))?;
        Ok(Some((
            create_device_on(Some(&adapter), D3D_DRIVER_TYPE_UNKNOWN)?,
            description,
        )))
    })();
    match hardware {
        Ok(Some(found)) => return Ok(found),
        Ok(None) => {}
        Err(message) => {
            log::stderr_text(&format!(
                "selftest: hardware device unavailable ({message}), falling back to WARP\n"
            ));
        }
    }
    Ok((
        create_device_on(None, D3D_DRIVER_TYPE_WARP)?,
        "WARP (software)".to_owned(),
    ))
}

/// IEEE 754 binary32 -> binary16 (round toward zero; good enough for a test pattern).
pub(crate) fn float_to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exponent = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let mut mantissa = bits & 0x7F_FFFF;

    if exponent >= 0x1F {
        return (sign | 0x7C00) as u16; // overflow / inf / nan
    }
    if exponent <= 0 {
        if exponent < -10 {
            return sign as u16; // underflow to zero
        }
        mantissa |= 0x80_0000;
        let sub = mantissa >> (1 - exponent + 13);
        return (sign | sub) as u16;
    }
    (sign | ((exponent as u32) << 10) | (mantissa >> 13)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_of_the_test_values_are_exact() {
        assert_eq!(float_to_half(0.0), 0x0000);
        assert_eq!(float_to_half(0.5), 0x3800);
        assert_eq!(float_to_half(1.0), 0x3C00);
        assert_eq!(float_to_half(2.5), 0x4100);
        assert_eq!(float_to_half(12.5), 0x4A40);
        assert_eq!(float_to_half(125.0), 0x57D0);
        assert_eq!(float_to_half(-1.0), 0xBC00);
        assert_eq!(float_to_half(1.0e9), 0x7C00);
    }

    /// The same check `capture --selftest` prints, on whatever Direct3D device this machine has (WARP when
    /// there is no GPU): the shader has to agree with the reference maths.
    #[test]
    fn the_gpu_shader_matches_the_reference_implementation() {
        let result = measure(80.0).expect("the conversion shader runs");
        for &(value, gpu, reference) in &result.luma {
            assert!(
                SelfTestResult::luma_ok(gpu, reference),
                "scRGB {value}: GPU {gpu}, reference {reference} ({})",
                result.device
            );
        }
        assert!(
            result.chroma_ok(),
            "chroma of grey: Cb={} Cr={}",
            result.cb,
            result.cr
        );
        // Textbook ST.2084 values at 80 nit: 1.0 -> 490, 2.5 -> 571, 125 -> 940 (clipped).
        let y10: Vec<i32> = result.luma.iter().map(|&(_, gpu, _)| gpu).collect();
        for (got, expected) in y10.iter().zip([64, 431, 490, 571, 723, 940]) {
            assert!((got - expected).abs() <= 2, "GPU luma {y10:?}");
        }
    }
}
