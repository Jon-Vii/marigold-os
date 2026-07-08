use crate::{FB_BYTES, HEIGHT, ROW_BYTES, WIDTH};

/// The upright drawing frame for one render: which way the device is held.
/// `set_pixel`/`pixel` take coordinates in this frame — x rightward, y
/// downward, exactly as the reader sees the screen — and map them onto the
/// panel's native row-major buffer. The mapping folds in the panel's
/// inverted row scan, which the renderers used to apply as whole-buffer
/// post-passes (`flip_vertical`, `rotate_180`) after drawing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FbFrame {
    /// Raw buffer coordinates, no transform. Boot fallbacks and byte-level
    /// tooling; not a frame the reader ever holds upright.
    #[default]
    Native,
    /// The default landscape hold.
    Landscape,
    /// The device rotated 180 degrees.
    LandscapeFlipped,
    /// The device rotated a quarter turn counter-clockwise: the long axis
    /// runs vertically, the front-button column sits below the screen.
    Portrait,
}

impl FbFrame {
    /// Drawing-frame width: the panel's long side lies horizontal in
    /// landscape, vertical in portrait.
    pub const fn width(self) -> usize {
        match self {
            FbFrame::Portrait => HEIGHT,
            _ => WIDTH,
        }
    }

    pub const fn height(self) -> usize {
        match self {
            FbFrame::Portrait => WIDTH,
            _ => HEIGHT,
        }
    }
}

// repr(C) pins the layout to FB_BYTES + 1 (both fields align 1): the
// firmware links one Framebuffer into an exactly-sized linker slot
// (fw/build.rs prev_fb_bytes), which must track this size.
#[repr(C)]
pub struct Framebuffer {
    data: [u8; FB_BYTES],
    frame: FbFrame,
}

impl Framebuffer {
    pub const fn new() -> Self {
        Self {
            data: [0xFF; FB_BYTES],
            frame: FbFrame::Native,
        }
    }

    #[inline]
    pub fn bytes(&self) -> &[u8; FB_BYTES] {
        &self.data
    }

    /// Set the drawing frame for the render about to happen. Framebuffers
    /// are long-lived statics, so every render sets its frame rather than
    /// trusting whatever the previous frame drew with.
    pub fn set_frame(&mut self, frame: FbFrame) {
        self.frame = frame;
    }

    pub const fn frame(&self) -> FbFrame {
        self.frame
    }

    pub const fn frame_width(&self) -> usize {
        self.frame.width()
    }

    pub const fn frame_height(&self) -> usize {
        self.frame.height()
    }

    pub fn clear(&mut self, white: bool) {
        self.data.fill(if white { 0xFF } else { 0x00 });
    }

    pub fn copy_from(&mut self, other: &Self) {
        self.data.copy_from_slice(other.bytes());
    }

    pub fn band(&self, y: usize, rows: usize) -> &[u8] {
        let start = y * ROW_BYTES;
        let end = start + rows.min(HEIGHT - y) * ROW_BYTES;
        &self.data[start..end]
    }

    /// Frame coordinates → native buffer coordinates. `None` when outside
    /// the frame. The landscape arms reproduce, pixel for pixel, what the
    /// old draw-then-flip pipeline wrote; the portrait arm is the quarter
    /// turn composed with the same scan inversion.
    #[inline]
    fn map(&self, x: usize, y: usize) -> Option<(usize, usize)> {
        match self.frame {
            FbFrame::Native => (x < WIDTH && y < HEIGHT).then_some((x, y)),
            FbFrame::Landscape => (x < WIDTH && y < HEIGHT).then(|| (x, HEIGHT - 1 - y)),
            FbFrame::LandscapeFlipped => (x < WIDTH && y < HEIGHT).then(|| (WIDTH - 1 - x, y)),
            FbFrame::Portrait => {
                (x < HEIGHT && y < WIDTH).then(|| (WIDTH - 1 - y, HEIGHT - 1 - x))
            }
        }
    }

    #[inline]
    pub fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return;
        };

        let index = native_y * ROW_BYTES + native_x / 8;
        let mask = 0x80 >> (native_x & 7);
        if white {
            self.data[index] |= mask;
        } else {
            self.data[index] &= !mask;
        }
    }

    /// Read a pixel in raw buffer coordinates, ignoring the drawing frame.
    /// For tooling that serializes the buffer (PNG dumps, canvas blits,
    /// panel models) and must not re-apply the frame transform on the way
    /// out.
    #[inline]
    pub fn native_pixel(&self, x: usize, y: usize) -> bool {
        if x >= WIDTH || y >= HEIGHT {
            return true;
        }
        let index = y * ROW_BYTES + x / 8;
        let mask = 0x80 >> (x & 7);
        self.data[index] & mask != 0
    }

    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> bool {
        let Some((native_x, native_y)) = self.map(x, y) else {
            return true;
        };

        let index = native_y * ROW_BYTES + native_x / 8;
        let mask = 0x80 >> (native_x & 7);
        self.data[index] & mask != 0
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

    /// The Landscape frame must write the same bytes the old pipeline
    /// produced by drawing in native coordinates and then flipping
    /// vertically (swapping row y with HEIGHT - 1 - y).
    #[test]
    fn landscape_frame_matches_draw_then_flip() {
        let mut framed = Framebuffer::new();
        framed.set_frame(FbFrame::Landscape);
        let mut flipped = Framebuffer::new();

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            flipped.set_pixel(*x, HEIGHT - 1 - y, false);
        }

        assert_eq!(framed.bytes()[..], flipped.bytes()[..]);
    }

    /// LandscapeFlipped must match the old draw + flip_vertical +
    /// rotate_180 composition, which reduces to mirroring x.
    #[test]
    fn landscape_flipped_frame_matches_draw_flip_rotate() {
        let mut framed = Framebuffer::new();
        framed.set_frame(FbFrame::LandscapeFlipped);
        let mut mirrored = Framebuffer::new();

        for (i, x) in [0usize, 13, 400, 401, WIDTH - 1].iter().enumerate() {
            let y = i * 123 % HEIGHT;
            framed.set_pixel(*x, y, false);
            mirrored.set_pixel(WIDTH - 1 - x, y, false);
        }

        assert_eq!(framed.bytes()[..], mirrored.bytes()[..]);
    }

    #[test]
    fn portrait_frame_swaps_dimensions_and_maps_corners() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        assert_eq!(fb.frame_width(), HEIGHT);
        assert_eq!(fb.frame_height(), WIDTH);

        // Frame top-left, top-right, bottom-left, bottom-right.
        fb.set_pixel(0, 0, false);
        fb.set_pixel(HEIGHT - 1, 0, false);
        fb.set_pixel(0, WIDTH - 1, false);
        fb.set_pixel(HEIGHT - 1, WIDTH - 1, false);

        let mut expected = Framebuffer::new();
        expected.set_pixel(WIDTH - 1, HEIGHT - 1, false);
        expected.set_pixel(WIDTH - 1, 0, false);
        expected.set_pixel(0, HEIGHT - 1, false);
        expected.set_pixel(0, 0, false);

        assert_eq!(fb.bytes()[..], expected.bytes()[..]);
    }

    #[test]
    fn portrait_frame_round_trips_reads() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        for (i, x) in [3usize, 99, 200, HEIGHT - 2].iter().enumerate() {
            fb.set_pixel(*x, i * 331 % WIDTH, false);
        }
        for (i, x) in [3usize, 99, 200, HEIGHT - 2].iter().enumerate() {
            assert!(!fb.pixel(*x, i * 331 % WIDTH));
        }
    }

    #[test]
    fn out_of_frame_writes_are_dropped() {
        let mut fb = Framebuffer::new();
        fb.set_frame(FbFrame::Portrait);
        let before = *fb.bytes();
        // Legal in landscape, outside the portrait frame's width.
        fb.set_pixel(HEIGHT, 10, false);
        fb.set_pixel(10, WIDTH, false);
        assert_eq!(fb.bytes()[..], before[..]);
        assert!(fb.pixel(HEIGHT, 10));
    }
}
