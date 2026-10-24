//! Painting glyphs a row at a time.
//!
//! A glyph arrives as rows of up to 64 pixels, top to bottom, each row a
//! `u64` with the glyph's first column in the most significant bit. Set bits
//! are ink; clear bits leave the framebuffer alone. Every frame gets whole
//! byte writes instead of one `set_pixel` per ink pixel:
//!
//! - `Native` and `Landscape` keep a glyph row a native row, so a row is a
//!   shift and two or three masked byte stores.
//! - `LandscapeFlipped` mirrors each row through the bit-reversal table and
//!   lands it the same way, counted from the other end.
//! - `Portrait` turns rows into native columns: eight frame rows share one
//!   native byte column, so the painter gathers up to eight rows and
//!   transposes them eight columns at a time, one byte per native row.
//!
//! A glyph that is not wholly inside the frame falls back to `set_pixel` per
//! ink pixel with the same clipping `font::draw_text` always had: pixels left
//! of or above the frame are dropped, and so is anything past its far edges.
//! Whatever the path, the framebuffer ends up exactly as that per-pixel loop
//! would leave it; the tests in `font.rs` hold every built-in glyph to that.

use super::{blit::transpose8, FbFrame, Framebuffer};
use crate::epd::REVERSE_BITS_LUT;
use crate::{HEIGHT, ROW_BYTES, WIDTH};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    /// `Native` or `Landscape`: a glyph row is a native row.
    Rows { flip_y: bool },
    /// `LandscapeFlipped`: a native row, mirrored.
    Mirrored,
    /// `Portrait`: eight frame rows share a native byte column.
    Columns,
    /// Not wholly inside the frame: one `set_pixel` per ink pixel.
    Clipped,
}

/// Paints one glyph's rows into a framebuffer. Feed every row with
/// [`Self::row`], top to bottom, then call [`Self::finish`].
pub struct GlyphPainter<'f> {
    fb: &'f mut Framebuffer,
    x: i32,
    y: i32,
    width: u32,
    white: bool,
    path: Path,
    /// Portrait: the native byte column being gathered (frame row / 8), and
    /// its rows by frame row % 8.
    group: i32,
    pending: [u64; 8],
}

impl<'f> GlyphPainter<'f> {
    /// A glyph box `width` x `height` (`width` at most 64) whose top-left
    /// pixel sits at frame (`x`, `y`). Ink is painted white or black.
    #[inline(always)]
    pub fn new(
        fb: &'f mut Framebuffer,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        white: bool,
    ) -> Self {
        let inside = x >= 0
            && y >= 0
            && x + width as i32 <= fb.frame_width() as i32
            && y + height as i32 <= fb.frame_height() as i32;
        // Any start bit (0..=7) plus the width must fit one u64 on the
        // row paths; wider glyphs take the per-pixel path.
        let fits = width <= 57;
        let path = match fb.frame {
            _ if !inside || !fits => Path::Clipped,
            FbFrame::Native => Path::Rows { flip_y: false },
            FbFrame::Landscape => Path::Rows { flip_y: true },
            FbFrame::LandscapeFlipped => Path::Mirrored,
            FbFrame::Portrait => Path::Columns,
        };
        Self {
            fb,
            x,
            y,
            width,
            white,
            path,
            group: -1,
            pending: [0; 8],
        }
    }

    /// Paint glyph row `row` (0 at the top); `bits` holds its pixels from
    /// bit 63 down.
    #[inline(always)]
    pub fn row(&mut self, row: u32, bits: u64) {
        let frame_y = self.y + row as i32;
        match self.path {
            Path::Rows { flip_y } => {
                if bits != 0 {
                    let native_y = if flip_y {
                        HEIGHT - 1 - frame_y as usize
                    } else {
                        frame_y as usize
                    };
                    self.fb.paint_native_row(
                        self.x as usize,
                        native_y,
                        bits,
                        self.width,
                        self.white,
                    );
                }
            }
            Path::Mirrored => {
                if bits != 0 {
                    // Frame column x is native WIDTH - 1 - x: the row lands
                    // reversed, its last column first.
                    let native_x = WIDTH - self.x as usize - self.width as usize;
                    let reversed = reverse_row(bits, self.width);
                    self.fb.paint_native_row(
                        native_x,
                        frame_y as usize,
                        reversed,
                        self.width,
                        self.white,
                    );
                }
            }
            Path::Columns => {
                let group = frame_y >> 3;
                if group != self.group {
                    self.flush_columns();
                    self.group = group;
                }
                self.pending[(frame_y & 7) as usize] = bits;
            }
            Path::Clipped => self.clipped_row(frame_y, bits),
        }
    }

    /// Paint whatever the portrait path still holds.
    #[inline(always)]
    pub fn finish(mut self) {
        if self.path == Path::Columns {
            self.flush_columns();
        }
    }

    /// Portrait: frame (x, y) is native (WIDTH - 1 - y, HEIGHT - 1 - x), so
    /// frame rows 8g..8g + 8 are native byte column WIDTH / 8 - 1 - g with
    /// frame row 8g + b in bit b, and glyph column c is native row
    /// HEIGHT - 1 - x - c.
    #[inline(always)]
    fn flush_columns(&mut self) {
        if self.group < 0 {
            return;
        }
        let byte_column = WIDTH / 8 - 1 - self.group as usize;
        let first_row = HEIGHT - 1 - self.x as usize;
        let rows = &self.pending;
        let mut chunk = 0;
        while chunk < self.width {
            let shift = 56 - chunk;
            let at = |bit: usize| ((rows[bit] >> shift) & 0xFF) as u32;
            // Rows in reverse, so the transpose puts row `bit` in bit `bit`.
            let high = at(7) << 24 | at(6) << 16 | at(5) << 8 | at(4);
            let low = at(3) << 24 | at(2) << 16 | at(1) << 8 | at(0);
            if high | low != 0 {
                let columns = transpose8(high, low);
                let count = (self.width - chunk).min(8) as usize;
                for (i, &bits) in columns[..count].iter().enumerate() {
                    if bits != 0 {
                        let native_y = first_row - chunk as usize - i;
                        self.fb
                            .paint_native_byte(native_y, byte_column, bits, self.white);
                    }
                }
            }
            chunk += 8;
        }
        self.pending = [0; 8];
    }

    #[cold]
    #[inline(never)]
    fn clipped_row(&mut self, frame_y: i32, bits: u64) {
        if frame_y < 0 {
            return;
        }
        for column in 0..self.width.min(64) {
            if bits & (1u64 << (63 - column)) != 0 {
                let frame_x = self.x + column as i32;
                if frame_x >= 0 {
                    self.fb
                        .set_pixel(frame_x as usize, frame_y as usize, self.white);
                }
            }
        }
    }
}

/// Mirror the first `width` columns of an MSB-first row.
#[inline(always)]
fn reverse_row(bits: u64, width: u32) -> u64 {
    let reverse = |word: u32| -> u32 {
        let b = word.to_be_bytes();
        u32::from_le_bytes([
            REVERSE_BITS_LUT[b[0] as usize],
            REVERSE_BITS_LUT[b[1] as usize],
            REVERSE_BITS_LUT[b[2] as usize],
            REVERSE_BITS_LUT[b[3] as usize],
        ])
    };
    if width <= 32 {
        ((reverse((bits >> 32) as u32) << (32 - width)) as u64) << 32
    } else {
        let whole = ((reverse(bits as u32) as u64) << 32) | reverse((bits >> 32) as u32) as u64;
        whole << (64 - width)
    }
}

impl Framebuffer {
    /// Paint the set bits of `bits` (MSB-first, `width` columns) into native
    /// row `native_y` from native column `native_x`. The caller keeps the row
    /// inside the buffer and `native_x % 8 + width` within 64.
    #[inline(always)]
    fn paint_native_row(
        &mut self,
        native_x: usize,
        native_y: usize,
        bits: u64,
        width: u32,
        white: bool,
    ) {
        let shift = (native_x & 7) as u32;
        let start = native_y * ROW_BYTES + (native_x >> 3);
        let count = ((shift + width + 7) >> 3) as usize;
        let high = (bits >> 32) as u32;
        let low = bits as u32;
        // The row shifted right by `shift`, as two big-endian words.
        let words = [
            high >> shift,
            (low >> shift) | ((high << 1) << (31 - shift)),
        ];
        for (k, byte) in self.data[start..start + count].iter_mut().enumerate() {
            let mask = (words[k >> 2] >> (24 - 8 * (k as u32 & 3))) as u8;
            if white {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
    }

    /// Paint the set bits of one native byte.
    #[inline(always)]
    fn paint_native_byte(&mut self, native_y: usize, byte_column: usize, bits: u8, white: bool) {
        let byte = &mut self.data[native_y * ROW_BYTES + byte_column];
        if white {
            *byte |= bits;
        } else {
            *byte &= !bits;
        }
    }
}
