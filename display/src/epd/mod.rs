//! Panel-controller drivers behind one compile-time seam.
//!
//! The shared surface is deliberately small: `RefreshMode` (the contract
//! with `app_core::RefreshPlanner` and the emulators — never forked per
//! panel), `SpiOp` for table-driven command sequences, and the
//! framebuffer-to-panel band transform. Everything controller-specific —
//! command bytes, init/update sequences, RAM-window math, waveform
//! handling — lives in the per-panel module re-exported here, so firmware
//! and tools import `display::epd::…` regardless of the selected device.

use crate::{fb::Framebuffer, BAND_BYTES, BAND_ROWS, HEIGHT, PORTRAIT_ROW_BYTES, ROW_BYTES, WIDTH};

/// Xteink X4: GDEQ0426T82 panel, SSD1677 controller. Also the panel the
/// desktop emulator simulates.
pub mod ssd1677;

/// Xteink X3: 792x528 panel, UC8253 controller. Skeleton only — see
/// docs/plans/2026-07-06-x3-support-plan.md, Phase 2.
#[cfg(feature = "device-x3")]
pub mod uc8253;

#[cfg(not(feature = "device-x3"))]
pub use ssd1677::*;
#[cfg(feature = "device-x3")]
pub use uc8253::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshMode {
    Full,
    Fast,
    /// One-flicker cleaning refresh: cleans ghosting in roughly half the
    /// full refresh time at a small contrast cost. Each controller
    /// realizes it differently (SSD1677: hot temperature-override OTP
    /// waveform; UC8253: half-scrub LUT bank).
    FastClean,
    PowerDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpiOp {
    Reset,
    WaitBusy,
    Command { cmd: u8, data: &'static [u8] },
}

/// Bit-reversal lookup. RV32IMC has no bit-manipulation extension, so
/// `u8::reverse_bits` lowers to a shift/mask sequence; one rodata load
/// per byte is cheaper across the 96 K transforms of a full flush.
static REVERSE_BITS_LUT: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = (i as u8).reverse_bits();
        i += 1;
    }
    lut
};

/// Copy one band of `fb` into `out` in the panel's memory order. The
/// orientation constants are per-panel (const generics so the row loop
/// compiles branch-free, as the old per-panel consts did); each panel
/// module wraps this with its own `fill_transformed_band`.
pub(crate) fn fill_transformed_band_impl<
    const MIRROR_X: bool,
    const MIRROR_Y: bool,
    const REVERSE_BITS: bool,
>(
    fb: &Framebuffer,
    band_y: usize,
    out: &mut [u8; BAND_BYTES],
) -> usize {
    let rows = BAND_ROWS.min(HEIGHT - band_y);
    let len = rows * ROW_BYTES;

    if !MIRROR_X && !MIRROR_Y && !REVERSE_BITS {
        out[..len].copy_from_slice(fb.band(band_y, rows));
        return len;
    }

    #[inline(always)]
    fn panel_byte<const MX: bool, const RB: bool>(value: u8) -> u8 {
        if MX || RB {
            REVERSE_BITS_LUT[value as usize]
        } else {
            value
        }
    }

    for out_row in 0..rows {
        let panel_y = band_y + out_row;
        let src_y = if MIRROR_Y {
            HEIGHT - 1 - panel_y
        } else {
            panel_y
        };
        let src_row = fb.band(src_y, 1);
        let dst_row = &mut out[out_row * ROW_BYTES..(out_row + 1) * ROW_BYTES];
        if MIRROR_X {
            for (dst, src) in dst_row.iter_mut().zip(src_row.iter().rev()) {
                *dst = panel_byte::<MIRROR_X, REVERSE_BITS>(*src);
            }
        } else {
            for (dst, src) in dst_row.iter_mut().zip(src_row.iter()) {
                *dst = panel_byte::<MIRROR_X, REVERSE_BITS>(*src);
            }
        }
    }

    len
}

/// The portrait counterpart of `fill_transformed_band_impl`: gather one
/// band of panel rows out of a portrait-composed frame.
///
/// A portrait frame is composed viewer-upright at `HEIGHT x WIDTH` for the
/// posture with the front-key ladder along the bottom edge (the device
/// turned a quarter counter-clockwise from landscape). On the panel that
/// content must appear rotated a quarter clockwise, and portrait frames
/// skip the composition-side panel-mount mirror, so the landscape-
/// equivalent source pixel for panel `(x, y)` is portrait
/// `(HEIGHT - 1 - y, WIDTH - 1 - x)`. Each output byte gathers eight
/// single bits from eight consecutive portrait rows at one fixed bit
/// position — pure loads against the resident framebuffer, no temporary
/// beyond the caller's band buffer, then the same per-panel mirror/reverse
/// tail as the landscape transform.
pub(crate) fn fill_transposed_band_impl<
    const MIRROR_X: bool,
    const MIRROR_Y: bool,
    const REVERSE_BITS: bool,
>(
    fb: &Framebuffer,
    band_y: usize,
    out: &mut [u8; BAND_BYTES],
) -> usize {
    let rows = BAND_ROWS.min(HEIGHT - band_y);
    let len = rows * ROW_BYTES;
    let data = fb.bytes();

    for out_row in 0..rows {
        let panel_y = band_y + out_row;
        let src_y = if MIRROR_Y {
            HEIGHT - 1 - panel_y
        } else {
            panel_y
        };
        // The whole landscape-equivalent row reads one portrait column.
        let px = HEIGHT - 1 - src_y;
        let p_byte = px / 8;
        let p_mask = 0x80u8 >> (px & 7);
        let dst_row = &mut out[out_row * ROW_BYTES..(out_row + 1) * ROW_BYTES];
        for (j, dst) in dst_row.iter_mut().enumerate() {
            // Landscape source byte for this panel byte (reverse byte
            // order when the panel mirrors X, as the landscape path does).
            let src_byte = if MIRROR_X { ROW_BYTES - 1 - j } else { j };
            let lx0 = src_byte * 8;
            let mut value = 0u8;
            for bit in 0..8 {
                let py = WIDTH - 1 - (lx0 + bit);
                let white = data[py * PORTRAIT_ROW_BYTES + p_byte] & p_mask != 0;
                value = (value << 1) | white as u8;
            }
            *dst = if MIRROR_X || REVERSE_BITS {
                REVERSE_BITS_LUT[value as usize]
            } else {
                value
            };
        }
    }

    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BAND_ROWS, HEIGHT, WIDTH};

    /// A portrait frame must stream to the panel byte-for-byte as the
    /// landscape frame carrying the same content rotated a quarter turn
    /// clockwise and panel-mount mirrored — the closed form the transposed
    /// gather implements: `landscape(x, y) = portrait(HEIGHT-1-y, WIDTH-1-x)`.
    #[test]
    fn transposed_bands_match_the_equivalent_landscape_frame() {
        let mut portrait = Framebuffer::new();
        portrait.set_portrait(true);
        // An asymmetric scatter: corners, an axis-crossing diagonal, and a
        // pseudo-random speckle so no mirror/rotation confusion cancels out.
        portrait.set_pixel(0, 0, false);
        portrait.set_pixel(HEIGHT - 1, 0, false);
        portrait.set_pixel(0, WIDTH - 1, false);
        portrait.set_pixel(HEIGHT - 2, WIDTH - 3, false);
        for i in 0..200 {
            let x = (i * 37) % HEIGHT;
            let y = (i * 91 + i / 3) % WIDTH;
            portrait.set_pixel(x, y, false);
        }

        let mut landscape = Framebuffer::new();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                landscape.set_pixel(x, y, portrait.pixel(HEIGHT - 1 - y, WIDTH - 1 - x));
            }
        }

        let mut band_p = [0u8; BAND_BYTES];
        let mut band_l = [0u8; BAND_BYTES];
        let mut y = 0;
        while y < HEIGHT {
            let len_p = fill_transformed_band(&portrait, y, &mut band_p);
            let len_l = fill_transformed_band(&landscape, y, &mut band_l);
            assert_eq!(len_p, len_l, "band length at y={y}");
            assert_eq!(band_p[..len_p], band_l[..len_l], "band bytes at y={y}");
            y += BAND_ROWS;
        }
    }

    /// Pixel-exact addressing through the whole portrait pipeline: one ink
    /// pixel in each portrait corner lands in the panel band and position
    /// the landscape corner pipeline proves out.
    #[test]
    fn portrait_corners_land_where_the_landscape_corners_do() {
        let corners = [
            (0, 0),
            (HEIGHT - 1, 0),
            (0, WIDTH - 1),
            (HEIGHT - 1, WIDTH - 1),
        ];
        for (px, py) in corners {
            let mut portrait = Framebuffer::new();
            portrait.set_portrait(true);
            portrait.set_pixel(px, py, false);

            let mut landscape = Framebuffer::new();
            landscape.set_pixel(WIDTH - 1 - py, HEIGHT - 1 - px, false);

            let mut band_p = [0u8; BAND_BYTES];
            let mut band_l = [0u8; BAND_BYTES];
            let mut y = 0;
            while y < HEIGHT {
                fill_transformed_band(&portrait, y, &mut band_p);
                let len = fill_transformed_band(&landscape, y, &mut band_l);
                assert_eq!(
                    band_p[..len],
                    band_l[..len],
                    "corner ({px},{py}) mismatch in band at y={y}"
                );
                y += BAND_ROWS;
            }
        }
    }
}
