//! `Framebuffer::blit` against the obvious reference, a `set_pixel` per
//! pixel: random images of 1 to 100 pixels a side, odd widths and padded
//! strides, at unaligned positions and over every edge of the frame, drawn
//! a random band of rows at a time, in every frame, over a framebuffer of
//! random pixels that must keep every one outside the image.
//!
//! The panel is whichever this build is for. The gate runs display's tests
//! on the X4 only, so `ui/tests/image_blit.rs` includes this file to run it
//! with the X3 panel as well.

use display::fb::{FbFrame, Framebuffer, ImageRows};

const FRAMES: [FbFrame; 4] = [
    FbFrame::Native,
    FbFrame::Landscape,
    FbFrame::LandscapeFlipped,
    FbFrame::Portrait,
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform enough in `low..=high`.
    fn range(&mut self, low: i64, high: i64) -> i64 {
        low + (self.next() % (high - low + 1) as u64) as i64
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// What `blit` must leave: each pixel of the band set on its own.
fn reference(fb: &mut Framebuffer, x: i32, y: i32, rows: ImageRows<'_>) {
    let width = rows.width.min(rows.stride * 8);
    for (index, line) in rows.bits.chunks_exact(rows.stride).enumerate() {
        let frame_y = i64::from(y) + (rows.first_row + index) as i64;
        for column in 0..width {
            let frame_x = i64::from(x) + column as i64;
            if frame_x < 0 || frame_y < 0 {
                continue;
            }
            let white = line[column / 8] & (0x80 >> (column % 8)) != 0;
            fb.set_pixel(frame_x as usize, frame_y as usize, white);
        }
    }
}

/// A framebuffer of random pixels, in raw buffer coordinates.
fn noise(rng: &mut Rng) -> Framebuffer {
    let mut fb = Framebuffer::new();
    for y in 0..display::HEIGHT {
        for x in (0..display::WIDTH).step_by(64) {
            let bits = rng.next();
            for bit in 0..64 {
                fb.set_pixel(x + bit, y, bits >> bit & 1 != 0);
            }
        }
    }
    fb
}

/// A position for an image `size` long on a frame side `side` long: over
/// the start edge, over the end edge, or anywhere, a third of the time each.
fn position(rng: &mut Rng, size: usize, side: usize) -> i32 {
    let (size, side) = (size as i64, side as i64);
    (match rng.next() % 3 {
        0 => rng.range(-size - 2, 9),
        1 => rng.range(side - size - 9, side + 2),
        _ => rng.range(-2, side + 2),
    }) as i32
}

/// Draws an image a band at a time with `blit` and with the reference, on
/// copies of `base` in `frame`, and checks they agree byte for byte.
#[allow(clippy::too_many_arguments)]
fn check(
    base: &Framebuffer,
    frame: FbFrame,
    x: i32,
    y: i32,
    width: usize,
    stride: usize,
    bits: &[u8],
    bands: &[usize],
) {
    let mut actual = Framebuffer::new();
    let mut expected = Framebuffer::new();
    actual.copy_from(base);
    expected.copy_from(base);
    actual.set_frame(frame);
    expected.set_frame(frame);
    let mut first_row = 0;
    for &band in bands {
        let rows = ImageRows {
            width,
            stride,
            first_row,
            bits: &bits[first_row * stride..(first_row + band) * stride],
        };
        actual.blit(x, y, rows);
        reference(&mut expected, x, y, rows);
        first_row += band;
    }
    if actual.bytes()[..] != expected.bytes()[..] {
        let differ = actual
            .bytes()
            .iter()
            .zip(expected.bytes().iter())
            .position(|(a, e)| a != e)
            .unwrap();
        panic!(
            "{frame:?} at ({x}, {y}), {width} wide, stride {stride}, bands {bands:?}: \
             first differing byte {differ} (row {}, byte {})",
            differ / display::ROW_BYTES,
            differ % display::ROW_BYTES
        );
    }
}

/// Splits `height` rows into random bands.
fn bands(rng: &mut Rng, height: usize) -> Vec<usize> {
    let mut bands = Vec::new();
    let mut left = height;
    while left > 0 {
        let band = if rng.next().is_multiple_of(4) {
            left
        } else {
            rng.range(1, left as i64) as usize
        };
        bands.push(band);
        left -= band;
    }
    bands
}

#[test]
fn random_images_match_a_pixel_by_pixel_reference_in_every_frame() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let base = noise(&mut rng);
    for frame in FRAMES {
        let (frame_width, frame_height) = (frame.width(), frame.height());
        for _ in 0..600 {
            let width = rng.range(1, 100) as usize;
            let height = rng.range(1, 100) as usize;
            // Padding bytes as well as padding bits, which must not draw.
            let stride = width.div_ceil(8) + rng.range(0, 2) as usize;
            let bits = rng.bytes(stride * height);
            let x = position(&mut rng, width, frame_width);
            let y = position(&mut rng, height, frame_height);
            let bands = bands(&mut rng, height);
            check(&base, frame, x, y, width, stride, &bits, &bands);
        }
    }
}

#[test]
fn every_offset_and_edge_matches_the_reference() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let base = noise(&mut rng);
    for frame in FRAMES {
        let (frame_width, frame_height) = (frame.width() as i32, frame.height() as i32);
        for width in [1usize, 7, 8, 9, 15, 17, 63, 100] {
            let stride = width.div_ceil(8);
            let height = 11;
            let bits = rng.bytes(stride * height);
            // Each bit offset, and flush against or over each edge.
            let xs = (0..9).chain([-(width as i32), -3, -1]).chain([
                frame_width - width as i32,
                frame_width - 1,
                frame_width - 5,
            ]);
            for x in xs {
                for y in [0, 3, -5, -10, frame_height - 11, frame_height - 4] {
                    check(&base, frame, x, y, width, stride, &bits, &[4, 7]);
                }
            }
        }
    }
}

/// The sizes the device draws: the decode bench's 586 × 440 figure and a
/// 464 × 348 one, and a page-wide image, in the firmware's bands.
#[test]
fn page_sized_images_match_the_reference() {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let base = noise(&mut rng);
    for frame in FRAMES {
        for (width, height) in [(586usize, 440usize), (464, 348), (784, 451), (213, 300)] {
            let stride = width.div_ceil(8);
            let bits = rng.bytes(stride * height);
            let band = 7_920 / stride;
            let mut bands = vec![band; height / band];
            if height % band > 0 {
                bands.push(height % band);
            }
            let x = (frame.width() as i32 - width as i32) / 2;
            check(&base, frame, x, 11, width, stride, &bits, &bands);
        }
    }
}

#[test]
fn empty_and_part_rows_draw_nothing_extra() {
    let mut rng = Rng(0x5555_aaaa_1234_8765);
    let base = noise(&mut rng);
    let mut fb = Framebuffer::new();
    fb.copy_from(&base);
    fb.set_frame(FbFrame::Portrait);
    // No stride, no rows, a part row only: nothing to draw.
    for (stride, bits) in [(0usize, &[0u8; 4][..]), (2, &[][..]), (3, &[0u8, 0][..])] {
        fb.blit(
            5,
            5,
            ImageRows {
                width: 9,
                stride,
                first_row: 0,
                bits,
            },
        );
    }
    assert_eq!(fb.bytes()[..], base.bytes()[..]);
    // A width past the stride is cut to it, and a trailing part row is
    // ignored.
    let bits = rng.bytes(2 * 3 + 1);
    let mut expected = Framebuffer::new();
    expected.copy_from(&base);
    expected.set_frame(FbFrame::Portrait);
    let rows = ImageRows {
        width: 40,
        stride: 2,
        first_row: 2,
        bits: &bits,
    };
    fb.blit(17, -1, rows);
    reference(
        &mut expected,
        17,
        -1,
        ImageRows {
            width: 16,
            bits: &bits[..6],
            ..rows
        },
    );
    assert_eq!(fb.bytes()[..], expected.bytes()[..]);
}
