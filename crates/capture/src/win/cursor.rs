//! Decodes DXGI pointer shapes into the BGRA image the conversion shader composites.

pub(crate) const POINTER_SHAPE_MONOCHROME: u32 = 1;
pub(crate) const POINTER_SHAPE_COLOR: u32 = 2;
pub(crate) const POINTER_SHAPE_MASKED_COLOR: u32 = 4;

/// A decoded cursor: straight-alpha BGRA, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CursorImage {
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Converts a DXGI pointer shape into straight-alpha BGRA for the shader.
/// Convention: alpha 0 with red 255 means "invert the desktop pixel" (XOR cursors).
///
/// `shape_height` is the height DXGI reports (for monochrome shapes: AND mask plus XOR mask, i.e. twice the
/// cursor). Returns `None` for an unknown type or an empty shape.
pub(crate) fn decode_cursor(
    data: &[u8],
    shape_type: u32,
    shape_width: u32,
    shape_height: u32,
    pitch: u32,
) -> Option<CursorImage> {
    let width = shape_width as usize;
    let height = if shape_type == POINTER_SHAPE_MONOCHROME {
        shape_height as usize / 2
    } else {
        shape_height as usize
    };
    let pitch = pitch as usize;
    if width == 0 || height == 0 || pitch == 0 {
        return None;
    }

    let mut bgra = vec![0u8; width * height * 4];
    match shape_type {
        POINTER_SHAPE_COLOR => {
            for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
                let Some(src) = data.get(y * pitch..y * pitch + width * 4) else {
                    break;
                };
                row.copy_from_slice(src);
                // The shader reads "alpha 0 with red set" as "invert the desktop pixel" (that is how
                // XOR cursors are carried). Cursors converted from .ico often store 0x00FFFFFF in the
                // transparent area, which would paint an inverted block around the pointer, so the
                // colour of every fully transparent pixel is cleared here.
                for pixel in row.chunks_exact_mut(4) {
                    if pixel[3] == 0 {
                        pixel[..3].fill(0);
                    }
                }
            }
        }
        POINTER_SHAPE_MASKED_COLOR => {
            // alpha 0x00: opaque colour. alpha 0xFF: XOR with the desktop.
            for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
                let Some(src) = data.get(y * pitch..y * pitch + width * 4) else {
                    break;
                };
                for (pixel, s) in row.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    let (b, g, r, a) = (s[0], s[1], s[2], s[3]);
                    if a == 0 {
                        pixel.copy_from_slice(&[b, g, r, 255]);
                    } else {
                        let invert = (r | g | b) != 0;
                        pixel.copy_from_slice(&[0, 0, if invert { 255 } else { 0 }, 0]);
                    }
                }
            }
        }
        POINTER_SHAPE_MONOCHROME => {
            // Top half = AND mask, bottom half = XOR mask, 1 bit per pixel.
            let row_bytes = width.div_ceil(8);
            for (y, row) in bgra.chunks_exact_mut(width * 4).enumerate() {
                let and_row = y * pitch;
                let xor_row = (y + height) * pitch;
                if xor_row + row_bytes > data.len() {
                    break;
                }
                for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                    let mask = 0x80u8 >> (x & 7);
                    let and_bit = data[and_row + (x >> 3)] & mask != 0;
                    let xor_bit = data[xor_row + (x >> 3)] & mask != 0;
                    let decoded: [u8; 4] = if !and_bit {
                        let v = if xor_bit { 255 } else { 0 }; // opaque white / black
                        [v, v, v, 255]
                    } else if xor_bit {
                        [0, 0, 255, 0] // invert
                    } else {
                        [0, 0, 0, 0] // transparent
                    };
                    pixel.copy_from_slice(&decoded);
                }
            }
        }
        _ => return None,
    }
    Some(CursorImage {
        bgra,
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_cursors_lose_the_colour_of_fully_transparent_pixels() {
        // 2x1, pitch 12 (padding after the row): opaque red, then "transparent white" as .ico files store it.
        let data = [0, 0, 255, 255, 255, 255, 255, 0, 9, 9, 9, 9];
        let c = decode_cursor(&data, POINTER_SHAPE_COLOR, 2, 1, 12).unwrap();
        assert_eq!((c.width, c.height), (2, 1));
        assert_eq!(c.bgra, [0, 0, 255, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn masked_colour_cursors_turn_the_xor_part_into_the_invert_marker() {
        // alpha 0 = opaque colour; alpha 0xFF with colour = invert; alpha 0xFF black = leave the desktop.
        let data = [10, 20, 30, 0, 255, 255, 255, 255, 0, 0, 0, 255];
        let c = decode_cursor(&data, POINTER_SHAPE_MASKED_COLOR, 3, 1, 12).unwrap();
        assert_eq!(c.bgra, [10, 20, 30, 255, 0, 0, 255, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn monochrome_cursors_are_an_and_mask_over_an_xor_mask() {
        // 4x1 cursor (shape height 2), pitch 1. AND bits 0011, XOR bits 0101:
        // black, white, transparent, invert.
        let data = [0b0011_0000, 0b0101_0000];
        let c = decode_cursor(&data, POINTER_SHAPE_MONOCHROME, 4, 2, 1).unwrap();
        assert_eq!((c.width, c.height), (4, 1));
        assert_eq!(
            c.bgra,
            [0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 255, 0]
        );
    }

    #[test]
    fn unknown_or_empty_shapes_are_ignored_and_short_data_does_not_panic() {
        assert!(decode_cursor(&[0; 16], 3, 2, 2, 8).is_none());
        assert!(decode_cursor(&[0; 16], POINTER_SHAPE_COLOR, 0, 2, 8).is_none());
        let c = decode_cursor(&[255; 8], POINTER_SHAPE_COLOR, 2, 2, 8).unwrap();
        assert_eq!(&c.bgra[..8], &[255; 8]);
        assert_eq!(&c.bgra[8..], &[0; 8]);
        assert!(decode_cursor(&[0b1111_0000], POINTER_SHAPE_MONOCHROME, 4, 2, 1).is_some());
    }
}
