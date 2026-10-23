//! Floyd–Steinberg dithering from gray rows to the panel's 1 bpp.
//!
//! Images are dithered once, when they are decoded, and the bits are stored,
//! so every draw of a page shows the same pattern. Rows alternate direction
//! (serpentine scanning), which keeps the diffused error from streaking to
//! one side. Output rows are packed like the framebuffer (`display/src/fb.rs`):
//! most significant bit leftmost, 1 = white, 0 = black, and the padding bits
//! after the last pixel white.
//!
//! The error carried between rows lives in two rows of `i16` cells carved
//! from caller scratch, one cell per pixel plus a spare at each end so that
//! error pushed past an edge needs no branch. A cell holds the sum of the
//! weighted errors (7, 3, 5 and 1 sixteenths) still in units of 1/16, so the
//! only division is a rounding shift per pixel. Errors stay within ±255 per
//! pixel, so a cell never exceeds ±16 · 255 and fits `i16`.
//!
//! Before diffusion, gray levels are stretched so that [`WHITE_POINT`] and
//! everything lighter becomes pure white. Scanned illustrations carry an
//! off-white paper tone, which error diffusion would otherwise render as a
//! sparse field of black dots across every background. The stretch is a
//! multiply and a shift, so it adds no division per pixel.

/// The gray level, and every lighter one, that dithers to pure white.
/// Chosen on Rackham's plates for *Alice*: 255 left paper dots, 240 gives
/// clean backgrounds with the faintest hatching intact, 224 starts to thin it.
pub const WHITE_POINT: u8 = 240;

/// `255 / WHITE_POINT` in 8.8 fixed point, rounded up so that
/// `WHITE_POINT` itself reaches 255.
const LEVEL_SCALE: u32 = (255u32 << 8).div_ceil(WHITE_POINT as u32);

/// A gray level after the white-point stretch.
#[inline(always)]
fn level(luma: u8) -> i32 {
    ((u32::from(luma) * LEVEL_SCALE) >> 8).min(255) as i32
}

/// Why a [`Ditherer`] refused its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DitherError {
    /// The width is zero.
    BadWidth,
    /// The scratch is smaller than [`Ditherer::scratch_bytes`].
    ScratchTooSmall,
    /// A luma row is not exactly the width, or the output is too short.
    BadRow,
}

/// Dithers a stream of gray rows, top to bottom, into packed 1 bpp rows.
pub struct Ditherer<'a> {
    /// Two rows of `width + 2` cells: the error arriving at the current row
    /// and the error being pushed to the next. They swap roles every row.
    cells: &'a mut [i16],
    width: usize,
    row: u32,
}

impl<'a> Ditherer<'a> {
    /// Scratch bytes a ditherer of `width` pixels needs: two error rows of
    /// `width + 2` cells at two bytes each, plus a byte to align them.
    pub const fn scratch_bytes(width: u32) -> usize {
        1 + 4 * (width as usize + 2)
    }

    /// Bytes in one packed output row.
    pub const fn packed_len(width: u32) -> usize {
        (width as usize).div_ceil(8)
    }

    /// A ditherer for rows of `width` pixels. Clears the error rows it
    /// carves from `scratch`, so the scratch may hold anything.
    pub fn new(scratch: &'a mut [u8], width: u32) -> Result<Self, DitherError> {
        if width == 0 {
            return Err(DitherError::BadWidth);
        }
        if scratch.len() < Self::scratch_bytes(width) {
            return Err(DitherError::ScratchTooSmall);
        }
        let count = 2 * (width as usize + 2);
        let (_, aligned, _) = bytemuck::pod_align_to_mut::<u8, i16>(scratch);
        let cells = aligned
            .get_mut(..count)
            .ok_or(DitherError::ScratchTooSmall)?;
        cells.fill(0);
        Ok(Self {
            cells,
            width: width as usize,
            row: 0,
        })
    }

    /// Dithers the next row. `luma` holds exactly the width in bytes; the
    /// packed row goes to the first [`Ditherer::packed_len`] bytes of `out`.
    pub fn row(&mut self, luma: &[u8], out: &mut [u8]) -> Result<(), DitherError> {
        let width = self.width;
        if luma.len() != width {
            return Err(DitherError::BadRow);
        }
        let out = out
            .get_mut(..width.div_ceil(8))
            .ok_or(DitherError::BadRow)?;
        // `new` carved exactly two rows, so this always splits.
        let Some((first, second)) = self.cells.split_at_mut_checked(width + 2) else {
            return Err(DitherError::ScratchTooSmall);
        };
        if self.row & 1 == 0 {
            diffuse_rightward(luma, first, second, out);
        } else {
            diffuse_leftward(luma, second, first, out);
        }
        self.row = self.row.wrapping_add(1);
        Ok(())
    }
}

/// One pixel of error diffusion. `incoming` is the cell's error from the row
/// above and `carry` the 7/16 share from the previous pixel in this row,
/// both in sixteenths. Returns whether the pixel is white and its error.
#[inline(always)]
fn quantize(luma: u8, incoming: i16, carry: i32) -> (bool, i32) {
    let value = level(luma) + ((i32::from(incoming) + carry + 8) >> 4);
    let white = value >= 128;
    (white, if white { value - 255 } else { value })
}

/// Dithers left to right. Pixel `x` reads cell `x + 1` of `above` and
/// finishes cell `x` of `below`. The cells below the current pixel and the
/// next one are still collecting error, so they ride in registers (`here`
/// and `ahead`) and each cell is written exactly once, which also means
/// `below` needs no clearing first.
fn diffuse_rightward(luma: &[u8], above: &[i16], below: &mut [i16], out: &mut [u8]) {
    let width = luma.len();
    let (Some(incoming), Some((finished, tail))) =
        (above.get(1..=width), below.split_at_mut_checked(width))
    else {
        return;
    };
    let mut outs = out.iter_mut();
    let (mut carry, mut here, mut ahead) = (0i32, 0i32, 0i32);
    let mut bits = 0u32;
    for (x, ((&value, &error), cell)) in luma
        .iter()
        .zip(incoming)
        .zip(finished.iter_mut())
        .enumerate()
    {
        let (white, e) = quantize(value, error, carry);
        *cell = (here + 3 * e) as i16;
        here = ahead + 5 * e;
        ahead = e;
        carry = 7 * e;
        bits = (bits << 1) | u32::from(white);
        if x & 7 == 7 {
            if let Some(byte) = outs.next() {
                *byte = bits as u8;
            }
            bits = 0;
        }
    }
    if let [below_last, past_edge, ..] = tail {
        *below_last = here as i16;
        *past_edge = ahead as i16;
    }
    let partial = width & 7;
    if partial != 0 {
        let pad = 8 - partial;
        if let Some(byte) = outs.next() {
            *byte = ((bits << pad) | ((1 << pad) - 1)) as u8;
        }
    }
}

/// Dithers right to left: the mirror image of [`diffuse_rightward`]. Pixel
/// `x` finishes cell `x + 2` of `below`, and bytes fill from the last one,
/// which starts with its padding bits already white.
fn diffuse_leftward(luma: &[u8], above: &[i16], below: &mut [i16], out: &mut [u8]) {
    let width = luma.len();
    let (Some(incoming), Some((head, finished))) =
        (above.get(1..=width), below.split_at_mut_checked(2))
    else {
        return;
    };
    let mut outs = out.iter_mut().rev();
    let (mut carry, mut here, mut ahead) = (0i32, 0i32, 0i32);
    let mut bits = match width & 7 {
        0 => 0,
        partial => 0xFFu32 >> partial,
    };
    for (x, ((&value, &error), cell)) in luma
        .iter()
        .zip(incoming)
        .zip(finished.iter_mut())
        .enumerate()
        .rev()
    {
        let (white, e) = quantize(value, error, carry);
        *cell = (here + 3 * e) as i16;
        here = ahead + 5 * e;
        ahead = e;
        carry = 7 * e;
        bits |= u32::from(white) << (7 - (x & 7));
        if x & 7 == 0 {
            if let Some(byte) = outs.next() {
                *byte = bits as u8;
            }
            bits = 0;
        }
    }
    if let [past_edge, below_first] = head {
        *below_first = here as i16;
        *past_edge = ahead as i16;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn dither(width: u32, rows: &[Vec<u8>], fill: u8) -> Vec<Vec<u8>> {
        let mut scratch = vec![fill; Ditherer::scratch_bytes(width)];
        let mut ditherer = Ditherer::new(&mut scratch, width).unwrap();
        rows.iter()
            .map(|row| {
                let mut out = vec![0x5A; Ditherer::packed_len(width)];
                ditherer.row(row, &mut out).unwrap();
                out
            })
            .collect()
    }

    /// The textbook algorithm with whole-image error arrays, for comparison.
    fn reference(width: usize, rows: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut errors = vec![vec![0i32; width + 2]; rows.len() + 1];
        let mut out = Vec::new();
        for (y, row) in rows.iter().enumerate() {
            let mut packed = vec![0xFFu8; width.div_ceil(8)];
            let order: Vec<usize> = if y % 2 == 0 {
                (0..width).collect()
            } else {
                (0..width).rev().collect()
            };
            let step: isize = if y % 2 == 0 { 1 } else { -1 };
            for x in order {
                let cell = x + 1;
                let value = level(row[x]) + ((errors[y][cell] + 8) >> 4);
                let white = value >= 128;
                let e = value - if white { 255 } else { 0 };
                if !white {
                    packed[x / 8] &= !(0x80 >> (x % 8));
                }
                let ahead = (cell as isize + step) as usize;
                let behind = (cell as isize - step) as usize;
                errors[y][ahead] += 7 * e;
                errors[y + 1][behind] += 3 * e;
                errors[y + 1][cell] += 5 * e;
                errors[y + 1][ahead] += e;
            }
            out.push(packed);
        }
        out
    }

    #[test]
    fn white_stays_white_and_black_stays_black() {
        for width in [1u32, 7, 8, 9, 16, 33, 800] {
            let white = dither(width, &vec![vec![255; width as usize]; 5], 0xA5);
            assert!(white.iter().flatten().all(|&b| b == 0xFF), "width {width}");

            let black = dither(width, &vec![vec![0; width as usize]; 5], 0xA5);
            let partial = width % 8;
            for row in &black {
                let (last, full) = row.split_last().unwrap();
                assert!(full.iter().all(|&b| b == 0), "width {width}");
                // Padding bits past the last pixel are white.
                let expected = if partial == 0 { 0 } else { 0xFF >> partial };
                assert_eq!(*last, expected, "width {width}");
            }
        }
    }

    #[test]
    fn the_white_point_and_lighter_dither_to_pure_white() {
        assert_eq!(level(WHITE_POINT), 255);
        assert_eq!(level(255), 255);
        assert_eq!(level(0), 0);
        assert!(level(WHITE_POINT - 1) < 255);
        // Monotonic, so no tone is ever darkened past a darker one.
        for luma in 1..=255u8 {
            assert!(level(luma) >= level(luma - 1), "{luma}");
        }
        let width = 64u32;
        for gray in WHITE_POINT..=255 {
            let rows = dither(width, &vec![vec![gray; width as usize]; 8], 0xA5);
            assert!(rows.iter().flatten().all(|&b| b == 0xFF), "gray {gray}");
        }
    }

    #[test]
    fn mid_gray_is_about_half_black() {
        let (width, height) = (64u32, 64usize);
        let rows = dither(width, &vec![vec![128; width as usize]; height], 0xA5);
        let white: u32 = rows.iter().flatten().map(|b| b.count_ones()).sum();
        let total = width * height as u32;
        let black = total - white;
        assert!(
            black * 100 >= total * 45 && black * 100 <= total * 55,
            "{black} of {total} black"
        );
    }

    #[test]
    fn packs_msb_first_with_white_padding() {
        // Pure black and white carry no error, so the bits are exact.
        let row = vec![255, 0, 255, 0, 0, 0, 0, 0, 0, 255];
        let out = dither(10, &[row.clone(), row], 0);
        assert_eq!(out[0], vec![0b1010_0000, 0b0111_1111]);
        // The second row runs right to left and must pack identically.
        assert_eq!(out[1], vec![0b1010_0000, 0b0111_1111]);

        assert_eq!(dither(3, &[vec![0, 255, 0]], 0), vec![vec![0b0101_1111]]);
        assert_eq!(dither(1, &[vec![0]], 0), vec![vec![0b0111_1111]]);
        assert_eq!(
            dither(8, &[vec![0, 0, 0, 0, 0, 0, 0, 255]], 0),
            vec![vec![0b0000_0001]]
        );
    }

    #[test]
    fn matches_the_textbook_algorithm_and_ignores_dirty_scratch() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for width in [1usize, 2, 3, 7, 8, 9, 31, 33, 100] {
            let rows: Vec<Vec<u8>> = (0..9)
                .map(|_| (0..width).map(|_| next() as u8).collect())
                .collect();
            let expected = reference(width, &rows);
            assert_eq!(dither(width as u32, &rows, 0), expected, "width {width}");
            assert_eq!(dither(width as u32, &rows, 0xA5), expected, "width {width}");
            assert_eq!(dither(width as u32, &rows, 0xFF), expected, "width {width}");
        }
    }

    #[test]
    fn is_deterministic() {
        let rows: Vec<Vec<u8>> = (0..20)
            .map(|y| (0..50).map(|x| ((x * 5 + y * 3) % 256) as u8).collect())
            .collect();
        assert_eq!(dither(50, &rows, 0xA5), dither(50, &rows, 0x00));
    }

    #[test]
    fn works_on_misaligned_scratch() {
        let rows: Vec<Vec<u8>> = (0..4).map(|y| vec![60 + y * 20; 21]).collect();
        let expected = dither(21, &rows, 0);
        let mut backing = vec![0xA5u8; Ditherer::scratch_bytes(21) + 1];
        for offset in 0..2 {
            let scratch = &mut backing[offset..offset + Ditherer::scratch_bytes(21)];
            let mut ditherer = Ditherer::new(scratch, 21).unwrap();
            for (row, want) in rows.iter().zip(&expected) {
                let mut out = [0u8; 3];
                ditherer.row(row, &mut out).unwrap();
                assert_eq!(&out[..], &want[..]);
            }
        }
    }

    #[test]
    fn rejects_bad_arguments() {
        let mut scratch = vec![0u8; Ditherer::scratch_bytes(10)];
        assert_eq!(
            Ditherer::new(&mut scratch, 0).err(),
            Some(DitherError::BadWidth)
        );
        assert_eq!(
            Ditherer::new(&mut scratch[1..], 10).err(),
            Some(DitherError::ScratchTooSmall)
        );
        let mut ditherer = Ditherer::new(&mut scratch, 10).unwrap();
        let mut out = [0u8; 2];
        assert_eq!(ditherer.row(&[0; 9], &mut out), Err(DitherError::BadRow));
        assert_eq!(
            ditherer.row(&[0; 10], &mut out[..1]),
            Err(DitherError::BadRow)
        );
        assert_eq!(ditherer.row(&[0; 10], &mut out), Ok(()));
    }
}
