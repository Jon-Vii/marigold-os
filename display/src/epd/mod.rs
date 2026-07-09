//! Panel-controller drivers behind one compile-time seam.
//!
//! The shared surface is deliberately small: `RefreshMode` (the contract
//! with `app_core::RefreshPlanner` and the emulators — never forked per
//! panel), `SpiOp` for table-driven command sequences, and the
//! framebuffer-to-panel band transform. Everything controller-specific —
//! command bytes, init/update sequences, RAM-window math, waveform
//! handling — lives in the per-panel module re-exported here, so firmware
//! and tools import `display::epd::…` regardless of the selected device.

use crate::{fb::Framebuffer, BAND_BYTES, BAND_ROWS, HEIGHT, ROW_BYTES};

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
