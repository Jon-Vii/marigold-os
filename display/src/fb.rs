use crate::{FB_BYTES, HEIGHT, PORTRAIT_ROW_BYTES, ROW_BYTES, WIDTH};

pub struct Framebuffer {
    data: [u8; FB_BYTES],
    /// Logical orientation of the composed frame. Landscape rows are
    /// `WIDTH` pixels; a portrait frame reuses the same bytes as `HEIGHT`
    /// pixel rows, `WIDTH` of them — the buffer size is identical either
    /// way. Portrait frames are composed viewer-upright (no panel-mount
    /// mirror); the flush transform transposes them into panel order.
    portrait: bool,
}

impl Framebuffer {
    pub const fn new() -> Self {
        Self {
            data: [0xFF; FB_BYTES],
            portrait: false,
        }
    }

    #[inline]
    pub fn bytes(&self) -> &[u8; FB_BYTES] {
        &self.data
    }

    #[inline]
    pub fn is_portrait(&self) -> bool {
        self.portrait
    }

    /// Switch the logical orientation. Existing contents keep their bytes
    /// but lose their meaning; callers re-compose from a `clear`.
    pub fn set_portrait(&mut self, portrait: bool) {
        self.portrait = portrait;
    }

    /// Logical frame width: the direction glyph rows run.
    #[inline]
    pub fn width(&self) -> usize {
        if self.portrait {
            HEIGHT
        } else {
            WIDTH
        }
    }

    /// Logical frame height: the number of composed rows.
    #[inline]
    pub fn height(&self) -> usize {
        if self.portrait {
            WIDTH
        } else {
            HEIGHT
        }
    }

    #[inline]
    fn row_bytes(&self) -> usize {
        if self.portrait {
            PORTRAIT_ROW_BYTES
        } else {
            ROW_BYTES
        }
    }

    pub fn clear(&mut self, white: bool) {
        self.data.fill(if white { 0xFF } else { 0x00 });
    }

    pub fn copy_from(&mut self, other: &Self) {
        self.data.copy_from_slice(other.bytes());
        self.portrait = other.portrait;
    }

    /// A run of physical landscape rows, as the panel flush streams them.
    /// Only meaningful for landscape frames; the portrait flush gathers
    /// transposed bytes instead of borrowing rows.
    pub fn band(&self, y: usize, rows: usize) -> &[u8] {
        let start = y * ROW_BYTES;
        let end = start + rows.min(HEIGHT - y) * ROW_BYTES;
        &self.data[start..end]
    }

    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        if x >= self.width() || y >= self.height() {
            return;
        }

        let index = y * self.row_bytes() + x / 8;
        let mask = 0x80 >> (x & 7);
        if white {
            self.data[index] |= mask;
        } else {
            self.data[index] &= !mask;
        }
    }

    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        if x >= self.width() || y >= self.height() {
            return true;
        }

        let index = y * self.row_bytes() + x / 8;
        let mask = 0x80 >> (x & 7);
        self.data[index] & mask != 0
    }

    /// Mirror across the logical horizontal axis: swap row `y` with row
    /// `height() - 1 - y`. In a row-major 1 bpp buffer this is pure row
    /// swapping, no bit work.
    pub fn flip_vertical(&mut self) {
        let row_bytes = self.row_bytes();
        let (top, bottom) = self.data.split_at_mut(FB_BYTES / 2);
        for (top_row, bottom_row) in top
            .chunks_exact_mut(row_bytes)
            .zip(bottom.chunks_exact_mut(row_bytes).rev())
        {
            top_row.swap_with_slice(bottom_row);
        }
    }

    /// Rotate the row-major 1 bpp buffer 180 degrees in place.
    pub fn rotate_180(&mut self) {
        for y in 0..HEIGHT / 2 {
            let top_start = y * ROW_BYTES;
            let bottom_start = (HEIGHT - 1 - y) * ROW_BYTES;
            let (head, tail) = self.data.split_at_mut(bottom_start);
            let top_row = &mut head[top_start..top_start + ROW_BYTES];
            let bottom_row = &mut tail[..ROW_BYTES];

            for (top, bottom) in top_row.iter_mut().zip(bottom_row.iter_mut().rev()) {
                let saved_top = *top;
                *top = bottom.reverse_bits();
                *bottom = saved_top.reverse_bits();
            }
        }

        if HEIGHT % 2 == 1 {
            let start = (HEIGHT / 2) * ROW_BYTES;
            let row = &mut self.data[start..start + ROW_BYTES];
            for index in 0..ROW_BYTES / 2 {
                let mirror = ROW_BYTES - 1 - index;
                let left = row[index];
                row[index] = row[mirror].reverse_bits();
                row[mirror] = left.reverse_bits();
            }
            if ROW_BYTES % 2 == 1 {
                let middle = ROW_BYTES / 2;
                row[middle] = row[middle].reverse_bits();
            }
        }
    }
}

impl Default for Framebuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flip_vertical_mirrors_rows() {
        let mut fb = Framebuffer::new();
        fb.set_pixel(0, 0, false);
        fb.set_pixel(799, 0, false);
        fb.set_pixel(13, 7, false);
        fb.set_pixel(400, 239, false);
        fb.set_pixel(401, 240, false);

        let mut expected = Framebuffer::new();
        expected.set_pixel(0, HEIGHT - 1, false);
        expected.set_pixel(799, HEIGHT - 1, false);
        expected.set_pixel(13, HEIGHT - 1 - 7, false);
        expected.set_pixel(400, HEIGHT - 1 - 239, false);
        expected.set_pixel(401, HEIGHT - 1 - 240, false);

        fb.flip_vertical();
        assert_eq!(fb.bytes()[..], expected.bytes()[..]);
    }

    #[test]
    fn portrait_addressing_swaps_the_axes() {
        let mut fb = Framebuffer::new();
        fb.set_portrait(true);
        assert_eq!(fb.width(), HEIGHT);
        assert_eq!(fb.height(), WIDTH);

        // In-bounds portrait pixels round-trip; the landscape-out-of-bounds
        // ones (y >= HEIGHT) must not be rejected by landscape limits.
        fb.set_pixel(0, WIDTH - 1, false);
        fb.set_pixel(HEIGHT - 1, 0, false);
        assert!(!fb.pixel(0, WIDTH - 1));
        assert!(!fb.pixel(HEIGHT - 1, 0));

        // Portrait bounds reject the landscape-only coordinates.
        fb.set_pixel(WIDTH - 1, 0, false);
        assert!(fb.pixel(WIDTH - 1, 0));

        // Row pitch is the portrait one: pixel (x, y) lives at
        // y * PORTRAIT_ROW_BYTES + x/8.
        let mut fresh = Framebuffer::new();
        fresh.set_portrait(true);
        fresh.set_pixel(9, 3, false);
        let index = 3 * PORTRAIT_ROW_BYTES + 1;
        assert_eq!(fresh.bytes()[index], !(0x80 >> 1));
    }

    #[test]
    fn portrait_flip_vertical_swaps_portrait_rows() {
        let mut fb = Framebuffer::new();
        fb.set_portrait(true);
        fb.set_pixel(5, 0, false);
        fb.set_pixel(HEIGHT - 1, 10, false);

        let mut expected = Framebuffer::new();
        expected.set_portrait(true);
        expected.set_pixel(5, WIDTH - 1, false);
        expected.set_pixel(HEIGHT - 1, WIDTH - 1 - 10, false);

        fb.flip_vertical();
        assert_eq!(fb.bytes()[..], expected.bytes()[..]);
    }

    #[test]
    fn copy_from_carries_the_orientation() {
        let mut portrait = Framebuffer::new();
        portrait.set_portrait(true);
        let mut copy = Framebuffer::new();
        copy.copy_from(&portrait);
        assert!(copy.is_portrait());
    }

    #[test]
    fn flip_vertical_twice_is_identity() {
        let mut fb = Framebuffer::new();
        for (i, x) in [3usize, 99, 200, 798].iter().enumerate() {
            fb.set_pixel(*x, i * 123 % HEIGHT, false);
        }
        let original = *fb.bytes();
        fb.flip_vertical();
        fb.flip_vertical();
        assert_eq!(fb.bytes()[..], original[..]);
    }

    #[test]
    fn rotate_180_maps_corners_and_points() {
        let mut fb = Framebuffer::new();
        fb.set_pixel(0, 0, false);
        fb.set_pixel(WIDTH - 1, 0, false);
        fb.set_pixel(13, 7, false);
        fb.set_pixel(400, 239, false);
        fb.set_pixel(401, 240, false);

        let mut expected = Framebuffer::new();
        expected.set_pixel(WIDTH - 1, HEIGHT - 1, false);
        expected.set_pixel(0, HEIGHT - 1, false);
        expected.set_pixel(WIDTH - 1 - 13, HEIGHT - 1 - 7, false);
        expected.set_pixel(WIDTH - 1 - 400, HEIGHT - 1 - 239, false);
        expected.set_pixel(WIDTH - 1 - 401, HEIGHT - 1 - 240, false);

        fb.rotate_180();
        assert_eq!(fb.bytes()[..], expected.bytes()[..]);
    }

    #[test]
    fn rotate_180_twice_is_identity() {
        let mut fb = Framebuffer::new();
        for (i, x) in [3usize, 99, 200, WIDTH - 2].iter().enumerate() {
            fb.set_pixel(*x, i * 123 % HEIGHT, false);
        }
        let original = *fb.bytes();
        fb.rotate_180();
        fb.rotate_180();
        assert_eq!(fb.bytes()[..], original[..]);
    }
}
