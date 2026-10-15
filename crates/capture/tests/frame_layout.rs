//! Checks the raw frame layout the capture engine writes against a real ffmpeg decode (ported from
//! tests/P010LayoutTests.cs). The compute shader and [`hdrcast_capture::color`] agree with each other by
//! construction; what they cannot prove on their own is that "p010le" / "nv12" really mean what the engine
//! assumes (plane order, 10 bits in the HIGH bits, interleaved UV, limited range). A silent mistake here would
//! show up only as wrong colours on the viewer's screen, so it is worth an integration test.
//!
//! Skipped (with a note) when ffmpeg is not installed: see `common::find_tool`.

mod common;

use hdrcast_capture::{color, CaptureFormat};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 16;

/// Builds a P010 frame whose columns hold known scRGB grey levels.
fn build_p010(scrgb_per_column: &[f64], sdr_white: f64) -> Vec<u8> {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut frame = vec![0u8; color::frame_bytes(CaptureFormat::P010, WIDTH, HEIGHT)];
    let uv = color::uv_plane_offset(CaptureFormat::P010, WIDTH, HEIGHT);
    let column_width = w / scrgb_per_column.len();

    for y in 0..h {
        for x in 0..w {
            let v = scrgb_per_column[(x / column_width).min(scrgb_per_column.len() - 1)];
            let (y10, cb10, cr10) = color::scrgb_to_yuv10(v, v, v, sdr_white);
            let y_offset = (y * w + x) * 2;
            frame[y_offset..y_offset + 2]
                .copy_from_slice(&color::to_p010_sample(y10).to_le_bytes());

            if y % 2 == 0 && x % 2 == 0 {
                let c = uv + (y / 2) * w * 2 + x * 2;
                frame[c..c + 2].copy_from_slice(&color::to_p010_sample(cb10).to_le_bytes());
                frame[c + 2..c + 4].copy_from_slice(&color::to_p010_sample(cr10).to_le_bytes());
            }
        }
    }
    frame
}

#[test]
fn p010_frame_decodes_to_the_expected_luminance() {
    let Some(ffmpeg) = common::find_tool("ffmpeg") else {
        return;
    };

    // 40, 80, 200 and 1000 nit columns.
    let columns = [0.5, 1.0, 2.5, 12.5];
    let dir = common::TempDir::new("p010");
    let raw = dir.file("frame.p010");
    std::fs::write(&raw, build_p010(&columns, 80.0)).unwrap();

    // Decode as p010le tagged PQ/BT.2020 and convert back to linear light with zscale, then read the
    // pixel values out as 32-bit float RGB. ffmpeg has to agree with us about the plane layout for this to work.
    let out = dir.file("out.rgb");
    let size = format!("{WIDTH}x{HEIGHT}");
    let (exit, _, log) = common::run(
        &ffmpeg,
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "p010le",
            "-video_size",
            &size,
            "-color_primaries",
            "bt2020",
            "-color_trc",
            "smpte2084",
            "-colorspace",
            "bt2020nc",
            "-color_range",
            "tv",
            "-i",
            raw.to_str().unwrap(),
            "-frames:v",
            "1",
            "-vf",
            "zscale=transferin=smpte2084:primariesin=bt2020:matrixin=bt2020nc:rangein=limited:\
             npl=10000:transfer=linear:primaries=bt709:matrix=gbr:range=full",
            "-pix_fmt",
            "gbrpf32le",
            "-f",
            "rawvideo",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(exit, 0, "ffmpeg failed: {log}");

    let planar = std::fs::read(&out).unwrap();
    assert_eq!(planar.len(), (WIDTH * HEIGHT * 4 * 3) as usize);

    // gbrpf32le: G plane, then B, then R. Read the green channel at the centre of each column.
    let column_width = WIDTH as usize / columns.len();
    for (i, column) in columns.iter().enumerate() {
        let x = i * column_width + column_width / 2;
        let index = (HEIGHT as usize / 2 * WIDTH as usize + x) * 4;
        let green = f32::from_le_bytes(planar[index..index + 4].try_into().unwrap());
        let expected_nits = column * 80.0;
        let got_nits = f64::from(green) * 10000.0; // linear output normalised to npl = 10000
        assert!(
            (got_nits - expected_nits).abs() <= f64::max(2.0, expected_nits * 0.03),
            "column {i} ({column} scRGB): expected ~{expected_nits:.1} nit, ffmpeg decoded {got_nits:.1} nit"
        );
    }
}

#[test]
fn nv12_frame_decodes_to_the_expected_srgb() {
    let Some(ffmpeg) = common::find_tool("ffmpeg") else {
        return;
    };

    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut frame = vec![0u8; color::frame_bytes(CaptureFormat::Nv12, WIDTH, HEIGHT)];
    let uv = color::uv_plane_offset(CaptureFormat::Nv12, WIDTH, HEIGHT);
    let columns = [0.0, 0.25, 0.5, 1.0]; // sRGB-encoded grey levels
    let column_width = w / columns.len();
    for y in 0..h {
        for x in 0..w {
            let v = columns[(x / column_width).min(columns.len() - 1)];
            let (y8, cb8, cr8) = color::srgb_to_yuv8(v, v, v);
            frame[y * w + x] = y8 as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let c = uv + (y / 2) * w + x;
                frame[c] = cb8 as u8;
                frame[c + 1] = cr8 as u8;
            }
        }
    }

    let dir = common::TempDir::new("nv12");
    let raw = dir.file("frame.nv12");
    std::fs::write(&raw, &frame).unwrap();
    let out = dir.file("out.rgb");
    let size = format!("{WIDTH}x{HEIGHT}");
    let (exit, _, log) = common::run(
        &ffmpeg,
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "-video_size",
            &size,
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
            "-color_range",
            "tv",
            "-i",
            raw.to_str().unwrap(),
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            out.to_str().unwrap(),
        ],
    );
    assert_eq!(exit, 0, "ffmpeg failed: {log}");

    let rgb = std::fs::read(&out).unwrap();
    assert_eq!(rgb.len(), w * h * 3);
    for (i, column) in columns.iter().enumerate() {
        let x = i * column_width + column_width / 2;
        let index = (h / 2 * w + x) * 3;
        let got = i32::from(rgb[index + 1]); // green
        let expected = (column * 255.0f64).round() as i32;
        assert!(
            (got - expected).abs() <= 3,
            "column {i}: expected sRGB ~{expected}, ffmpeg decoded {got}"
        );
    }
}
