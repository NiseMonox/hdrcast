//! Re-packing what a capture API hands over into the engine's layouts: picture planes whose rows carry
//! padding into the tightly packed frame of design.md section 2.2, the 4:4:4 full-range 10-bit format into
//! P010, and planar PCM into interleaved frames.
//!
//! Pure logic, compiled on every platform so that its tests run wherever the crate is developed; only the
//! macOS backend (ScreenCaptureKit hands over `CVPixelBuffer` planes and non-interleaved audio) calls it.

/// Copies a picture plane of `rows` rows into `dst` without the padding: in `src` the rows start `stride`
/// bytes apart and only the first `row_bytes` of each belong to the picture; `dst` gets exactly
/// `row_bytes * rows` bytes.
///
/// The last row of `src` may end right after its `row_bytes` (the padding of the final row need not be
/// mapped). Nothing is written when the sizes do not fit.
pub(crate) fn copy_plane(
    src: &[u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
    dst: &mut [u8],
) -> Result<(), &'static str> {
    if row_bytes == 0 || rows == 0 {
        return Err("the plane is empty");
    }
    if stride < row_bytes {
        return Err("the plane's rows are shorter than the picture is wide");
    }
    let needed = stride
        .checked_mul(rows - 1)
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or("the plane size overflows")?;
    if src.len() < needed {
        return Err("the plane holds fewer rows than the picture is high");
    }
    if row_bytes.checked_mul(rows) != Some(dst.len()) {
        return Err("the frame buffer does not match the plane");
    }
    if stride == row_bytes {
        // no padding at all: the plane is already packed
        dst.copy_from_slice(&src[..needed]);
        return Ok(());
    }
    for (dst_row, src_row) in dst.chunks_exact_mut(row_bytes).zip(src.chunks(stride)) {
        dst_row.copy_from_slice(&src_row[..row_bytes]);
    }
    Ok(())
}

/// Full-range 10-bit luma (0..=1023) to limited range (64..=940).
fn luma_full_to_limited(full: u32) -> u32 {
    64 + (full * 876 + 511) / 1023
}

/// The sum of four full-range 10-bit chroma samples (each 0..=1023, neutral 512) to one limited-range
/// sample (64..=960, neutral 512): `512 + (sum / 4 - 512) * 896 / 1023`, rounded.
fn chroma_sum4_full_to_limited(sum4: u32) -> u32 {
    // sum4 <= 4092, so every term stays far inside u32; the numerator is positive for sum4 = 0 as well
    // (512 * 4092 - 2048 * 896 + 2046 = 262_142).
    (512 * 4092 + sum4 * 896 + 2046 - 2048 * 896) / 4092
}

fn sample10(bytes: &[u8]) -> u32 {
    u32::from(u16::from_le_bytes([bytes[0], bytes[1]]) >> 6)
}

fn put10(bytes: &mut [u8], code10: u32) {
    bytes.copy_from_slice(&((code10 as u16) << 6).to_le_bytes());
}

/// Converts a 4:4:4 full-range 10-bit two-plane picture (CoreVideo `'xf44'`: plane 0 = Y, plane 1 =
/// interleaved Cb Cr at full resolution, every sample 16 bits little-endian with the 10 bits on top) into a
/// packed P010 frame: limited range, 4:2:0, chroma = the mean of each 2x2 block (what the Windows shader
/// does as well).
///
/// `luma_stride` / `chroma_stride` are the distances between rows in bytes; `width` and `height` are in
/// pixels and must be even. `dst` must be exactly one P010 frame (`width * height * 3` bytes).
pub(crate) fn xf44_to_p010(
    luma: &[u8],
    luma_stride: usize,
    chroma: &[u8],
    chroma_stride: usize,
    width: usize,
    height: usize,
    dst: &mut [u8],
) -> Result<(), &'static str> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err("the picture size must be even");
    }
    let luma_row = width * 2;
    let chroma_row = width * 4;
    if luma_stride < luma_row || chroma_stride < chroma_row {
        return Err("the plane's rows are shorter than the picture is wide");
    }
    if luma.len() < luma_stride * (height - 1) + luma_row
        || chroma.len() < chroma_stride * (height - 1) + chroma_row
    {
        return Err("the plane holds fewer rows than the picture is high");
    }
    if dst.len() != width * height * 3 {
        return Err("the frame buffer does not match the picture");
    }
    let (dst_luma, dst_chroma) = dst.split_at_mut(luma_row * height);

    for (dst_row, src_row) in dst_luma
        .chunks_exact_mut(luma_row)
        .zip(luma.chunks(luma_stride))
    {
        for (out, sample) in dst_row
            .chunks_exact_mut(2)
            .zip(src_row[..luma_row].chunks_exact(2))
        {
            put10(out, luma_full_to_limited(sample10(sample)));
        }
    }

    // One output row of W/2 (Cb, Cr) pairs per two input rows of W pairs.
    for (row, dst_row) in dst_chroma.chunks_exact_mut(luma_row).enumerate() {
        let upper = &chroma[chroma_stride * (row * 2)..][..chroma_row];
        let lower = &chroma[chroma_stride * (row * 2 + 1)..][..chroma_row];
        for ((out, top), bottom) in dst_row
            .chunks_exact_mut(4)
            .zip(upper.chunks_exact(8))
            .zip(lower.chunks_exact(8))
        {
            // each 8-byte block: Cb Cr Cb Cr of two neighbouring pixels
            let cb = sample10(&top[0..2])
                + sample10(&top[4..6])
                + sample10(&bottom[0..2])
                + sample10(&bottom[4..6]);
            let cr = sample10(&top[2..4])
                + sample10(&top[6..8])
                + sample10(&bottom[2..4])
                + sample10(&bottom[6..8]);
            put10(&mut out[0..2], chroma_sum4_full_to_limited(cb));
            put10(&mut out[2..4], chroma_sum4_full_to_limited(cr));
        }
    }
    Ok(())
}

/// Interleaves planar PCM - one slice of samples per channel, `bytes_per_sample` bytes each - into frames
/// in `out` (which is cleared first). Returns the number of frames: that of the shortest plane.
pub(crate) fn interleave_planes(
    planes: &[&[u8]],
    bytes_per_sample: usize,
    out: &mut Vec<u8>,
) -> usize {
    out.clear();
    if planes.is_empty() || bytes_per_sample == 0 {
        return 0;
    }
    let frames = planes
        .iter()
        .map(|plane| plane.len() / bytes_per_sample)
        .min()
        .unwrap_or(0);
    out.reserve(frames * planes.len() * bytes_per_sample);
    for frame in 0..frames {
        let at = frame * bytes_per_sample;
        for plane in planes {
            out.extend_from_slice(&plane[at..at + bytes_per_sample]);
        }
    }
    frames
}

/// A four-character code (pixel format, audio format) the way Apple's tools print it: `'420v'`; codes that
/// are not printable come out as hexadecimal.
pub(crate) fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        format!("0x{code:08x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::CaptureFormat;
    use crate::color;

    #[test]
    fn padded_rows_are_packed_and_unpadded_planes_are_copied_whole() {
        // 3 rows of 4 picture bytes, 6 bytes apart; the last row ends without its padding
        let src = [
            1, 2, 3, 4, 0xEE, 0xEE, //
            5, 6, 7, 8, 0xEE, 0xEE, //
            9, 10, 11, 12,
        ];
        let mut dst = [0u8; 12];
        copy_plane(&src, 6, 4, 3, &mut dst).unwrap();
        assert_eq!(dst, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        let mut same = [0u8; 12];
        copy_plane(&dst, 4, 4, 3, &mut same).unwrap();
        assert_eq!(same, dst);

        // a source that is longer than needed (a full last row, more rows) is fine too
        let long = [7u8; 64];
        let mut out = [0u8; 8];
        copy_plane(&long, 16, 4, 2, &mut out).unwrap();
        assert_eq!(out, [7; 8]);
    }

    #[test]
    fn a_plane_that_does_not_fit_is_refused_without_writing() {
        let src = [1u8; 16];
        let mut dst = [0u8; 12];
        // rows shorter than the picture
        assert!(copy_plane(&src, 3, 4, 3, &mut dst).is_err());
        // too few rows in the source: 6 * 2 + 4 = 16 fits, one more row does not
        assert!(copy_plane(&src, 6, 4, 4, &mut [0u8; 16]).is_err());
        // destination of the wrong size
        assert!(copy_plane(&src, 6, 4, 3, &mut [0u8; 11]).is_err());
        assert!(copy_plane(&src, 6, 0, 3, &mut []).is_err());
        assert_eq!(dst, [0; 12]);
    }

    fn le16(code10: u32) -> [u8; 2] {
        ((code10 as u16) << 6).to_le_bytes()
    }

    /// A 4:4:4 picture of one colour, with `pad` bytes of 0xFF after every row.
    fn xf44_flat(
        width: usize,
        height: usize,
        y: u32,
        cb: u32,
        cr: u32,
        pad: usize,
    ) -> (Vec<u8>, usize, Vec<u8>, usize) {
        let mut luma = Vec::new();
        let mut chroma = Vec::new();
        for _ in 0..height {
            for _ in 0..width {
                luma.extend_from_slice(&le16(y));
                chroma.extend_from_slice(&le16(cb));
                chroma.extend_from_slice(&le16(cr));
            }
            luma.extend(std::iter::repeat_n(0xFF, pad));
            chroma.extend(std::iter::repeat_n(0xFF, pad));
        }
        (luma, width * 2 + pad, chroma, width * 4 + pad)
    }

    fn p010_at(frame: &[u8], sample: usize) -> i32 {
        color::from_p010_sample(u16::from_le_bytes([
            frame[sample * 2],
            frame[sample * 2 + 1],
        ]))
    }

    #[test]
    fn full_range_444_becomes_limited_range_p010() {
        let (w, h) = (4usize, 2usize);
        let frame_bytes = color::frame_bytes(CaptureFormat::P010, w as u32, h as u32);
        let uv = color::uv_plane_offset(CaptureFormat::P010, w as u32, h as u32) / 2;
        // (full-range input, limited-range result): black, white, mid grey, and the chroma extremes
        for (y, cb, cr, y_out, cb_out, cr_out) in [
            (0, 512, 512, 64, 512, 512),
            (1023, 512, 512, 940, 512, 512),
            (512, 0, 1023, 502, 64, 960),
            (256, 1023, 0, 283, 960, 64),
        ] {
            for pad in [0usize, 10] {
                let (luma, ls, chroma, cs) = xf44_flat(w, h, y, cb, cr, pad);
                let mut frame = vec![0u8; frame_bytes];
                xf44_to_p010(&luma, ls, &chroma, cs, w, h, &mut frame).unwrap();
                for i in 0..w * h {
                    assert_eq!(p010_at(&frame, i), y_out, "luma {y} pad {pad}");
                }
                for pair in 0..(w / 2) * (h / 2) {
                    assert_eq!(p010_at(&frame, uv + pair * 2), cb_out, "cb {cb} pad {pad}");
                    assert_eq!(
                        p010_at(&frame, uv + pair * 2 + 1),
                        cr_out,
                        "cr {cr} pad {pad}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_range_conversion_matches_the_real_number_formula_everywhere() {
        for full in 0..=1023u32 {
            let exact = 64.0 + 876.0 * f64::from(full) / 1023.0;
            assert_eq!(
                luma_full_to_limited(full),
                exact.round() as u32,
                "luma {full}"
            );
        }
        for sum4 in 0..=4092u32 {
            let exact = 512.0 + (f64::from(sum4) / 4.0 - 512.0) * 896.0 / 1023.0;
            let got = f64::from(chroma_sum4_full_to_limited(sum4));
            // the integer form rounds half up; never more than half a code value away
            assert!((got - exact).abs() <= 0.5 + 1e-9, "chroma sum {sum4}");
        }
    }

    #[test]
    fn chroma_is_the_mean_of_each_two_by_two_block_and_cb_cr_stay_in_order() {
        let (w, h) = (4usize, 2usize);
        // Cb differs per pixel of the left block (mean 512), Cr is constant per block.
        let cb = [[312u32, 712, 100, 100], [512, 512, 100, 100]];
        let cr = [[800u32, 800, 200, 200], [800, 800, 200, 200]];
        let mut luma = Vec::new();
        let mut chroma = Vec::new();
        for row in 0..h {
            for x in 0..w {
                luma.extend_from_slice(&le16(512));
                chroma.extend_from_slice(&le16(cb[row][x]));
                chroma.extend_from_slice(&le16(cr[row][x]));
            }
        }
        let mut frame = vec![0u8; w * h * 3];
        xf44_to_p010(&luma, w * 2, &chroma, w * 4, w, h, &mut frame).unwrap();
        let uv = w * h;
        assert_eq!(
            p010_at(&frame, uv),
            512,
            "left block Cb: mean of 312 712 512 512"
        );
        assert_eq!(
            p010_at(&frame, uv + 1),
            764,
            "left block Cr: 512 + 288 * 896 / 1023"
        );
        assert_eq!(
            p010_at(&frame, uv + 2),
            151,
            "right block Cb: 512 - 412 * 896 / 1023"
        );
        assert_eq!(
            p010_at(&frame, uv + 3),
            239,
            "right block Cr: 512 - 312 * 896 / 1023"
        );
    }

    #[test]
    fn a_444_picture_that_does_not_fit_is_refused() {
        let (luma, ls, chroma, cs) = xf44_flat(4, 2, 512, 512, 512, 0);
        let mut frame = vec![0u8; 4 * 2 * 3];
        assert!(xf44_to_p010(&luma, ls, &chroma, cs, 3, 2, &mut frame).is_err());
        assert!(xf44_to_p010(&luma, ls - 2, &chroma, cs, 4, 2, &mut frame).is_err());
        assert!(xf44_to_p010(&luma[..10], ls, &chroma, cs, 4, 2, &mut frame).is_err());
        assert!(xf44_to_p010(&luma, ls, &chroma[..20], cs, 4, 2, &mut frame).is_err());
        assert!(xf44_to_p010(&luma, ls, &chroma, cs, 4, 2, &mut frame[1..]).is_err());
    }

    #[test]
    fn planar_pcm_is_interleaved_frame_by_frame() {
        let left: Vec<u8> = [1.0f32, 2.0, 3.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let right: Vec<u8> = [-1.0f32, -2.0, -3.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut out = vec![0xAA];
        assert_eq!(interleave_planes(&[&left, &right], 4, &mut out), 3);
        let samples: Vec<f32> = out
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(samples, [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);

        // the shortest plane decides; a trailing partial sample is ignored
        assert_eq!(interleave_planes(&[&left, &right[..9]], 4, &mut out), 2);
        assert_eq!(out.len(), 16);
        assert_eq!(interleave_planes(&[], 4, &mut out), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn four_character_codes_print_like_apples_tools() {
        assert_eq!(fourcc(0x3432_3076), "'420v'");
        assert_eq!(fourcc(0x7834_3230), "'x420'");
        assert_eq!(fourcc(0x7866_3434), "'xf44'");
        assert_eq!(fourcc(0x0000_0020), "0x00000020");
    }
}
