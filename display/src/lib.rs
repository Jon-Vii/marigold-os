#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(feature = "builtin-custom-font")]
pub mod custom_generated;
pub mod epd;
pub mod fb;
pub mod font;
pub mod literata_extra_generated;
pub mod literata_generated;
pub mod literata_semibold_generated;
pub mod literata_sizes_generated;
pub mod merriweather_generated;
pub mod render;

/// Xteink X4: GDEQ0426T82 4.26" panel, SSD1677 controller.
#[cfg(not(feature = "device-x3"))]
pub const WIDTH: usize = 800;
#[cfg(not(feature = "device-x3"))]
pub const HEIGHT: usize = 480;

/// Xteink X3: 3.68" panel, UC8253 controller.
#[cfg(feature = "device-x3")]
pub const WIDTH: usize = 792;
#[cfg(feature = "device-x3")]
pub const HEIGHT: usize = 528;

pub const ROW_BYTES: usize = WIDTH / 8;
pub const FB_BYTES: usize = ROW_BYTES * HEIGHT;
pub const BAND_ROWS: usize = 80;
pub const BAND_BYTES: usize = ROW_BYTES * BAND_ROWS;

// WIDTH must stay byte-addressable; HEIGHT need not divide into bands —
// fill_transformed_band already emits a short final band.
const _: () = assert!(WIDTH.is_multiple_of(8));

#[cfg(not(feature = "device-x3"))]
const _: () = assert!(FB_BYTES == 48_000 && BAND_BYTES == 8_000);
#[cfg(feature = "device-x3")]
const _: () = assert!(FB_BYTES == 52_272 && BAND_BYTES == 7_920);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const FULL: Self = Self {
        x: 0,
        y: 0,
        w: WIDTH as u16,
        h: HEIGHT as u16,
    };

    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn clipped(self) -> Option<Self> {
        let x0 = self.x.min(WIDTH as u16);
        let y0 = self.y.min(HEIGHT as u16);
        let x1 = self.x.saturating_add(self.w).min(WIDTH as u16);
        let y1 = self.y.saturating_add(self.h).min(HEIGHT as u16);

        if x1 <= x0 || y1 <= y0 {
            return None;
        }

        Some(Self {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        })
    }
}
