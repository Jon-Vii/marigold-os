//! Inverse DCTs from a block's dequantized coefficients to its pixels: the
//! full 8×8, and reduced ones that give each pixel the mean of the 2×2,
//! 4×4 or 8×8 square of full-size pixels it stands for.
//!
//! All are separable and fixed point with 13 fractional bits, in the
//! manner of libjpeg's `jidctint.c`: a pass down the columns, in place in
//! the block, keeps two extra bits; a pass along the rows writes pixels.
//! None divides. The arithmetic wraps rather than overflows: real data
//! stays far inside `i32`, and a hostile block only makes hostile pixels.
//!
//! The reduced transforms follow from the 8-point IDCT
//! `f(x) = ½ Σ C(u) F(u) cos((2x+1)uπ/16)`. Averaging it over pairs,
//! `(f(2x) + f(2x+1)) / 2 = ½ Σ C(u) F(u) cos(uπ/16) cos((2x+1)uπ/8)`, and
//! since `cos((2x+1)(8-u)π/8) = -cos((2x+1)uπ/8)` the terms for `u` and
//! `8 - u` fold into a 4-point IDCT of `G(u) = cos(uπ/16) F(u) -
//! sin(uπ/16) F(8-u)` (`F(4)` drops out). Over runs of four, only DC and
//! the odd terms survive, each weighted by `(cos(uπ/16) + cos(3uπ/16)) /
//! 2`; over all eight, only DC. Every 1-D transform here returns its
//! outputs times `2√2`, so two passes give eight times the pixel, which
//! the final shift removes along with the fixed-point bits.

use core::num::Wrapping;

type W = Wrapping<i32>;

const CONST_BITS: usize = 13;
const PASS1_BITS: usize = 2;
/// Pass 1 keeps `PASS1_BITS` of its `CONST_BITS`.
const PASS1_SHIFT: usize = CONST_BITS - PASS1_BITS;
/// Pass 2 drops the rest, and the factor of eight two passes leave.
const PASS2_SHIFT: usize = CONST_BITS + PASS1_BITS + 3;

// The 8-point transform's constants, `x · 2^13` rounded (libjpeg's names).
const FIX_0_298631336: i32 = 2446;
const FIX_0_390180644: i32 = 3196;
const FIX_0_541196100: i32 = 4433;
const FIX_0_765366865: i32 = 6270;
const FIX_0_899976223: i32 = 7373;
const FIX_1_175875602: i32 = 9633;
const FIX_1_501321110: i32 = 12299;
const FIX_1_847759065: i32 = 15137;
const FIX_1_961570560: i32 = 16069;
const FIX_2_053119869: i32 = 16819;
const FIX_2_562915447: i32 = 20995;
const FIX_3_072711026: i32 = 25172;

// Pair means. Even part: cos(π/8), sin(π/8).
const PAIR_C2: i32 = 7568;
const PAIR_S2: i32 = 3135;
// Odd part, first and last outputs: √2·cos(π/8) times cos(π/16), sin(π/16);
// √2·cos(3π/8) times cos(3π/16), sin(3π/16).
const PAIR_A1: i32 = 10498;
const PAIR_A7: i32 = 2088;
const PAIR_A3: i32 = 3686;
const PAIR_A5: i32 = 2463;
// Odd part, middle outputs: √2·cos(3π/8) times cos(π/16), sin(π/16);
// √2·cos(π/8) times cos(3π/16), sin(3π/16).
const PAIR_B1: i32 = 4348;
const PAIR_B7: i32 = 865;
const PAIR_B3: i32 = 8900;
const PAIR_B5: i32 = 5946;
// Means of four: (cos(uπ/16) + cos(3uπ/16)) / 2 for u = 1, 3, 5, 7, the
// last two negated.
const QUAD_1: i32 = 7423;
const QUAD_3: i32 = 2607;
const QUAD_5: i32 = 1742;
const QUAD_7: i32 = 1477;

/// Writes a block's pixels into `out`, rows `stride` apart, `side` (1, 2,
/// 4 or 8) to a side, and leaves the block zeroed for the next. `ac` says
/// whether any AC coefficient is non-zero; a block without is one flat
/// value, worked out as every transform here would.
pub(super) fn transform(
    block: &mut [i32; 64],
    quant: &[u16; 64],
    ac: bool,
    side: u32,
    out: &mut [u8],
    stride: usize,
) {
    if !ac {
        let dc = dequantize(block, quant, 0);
        let value = pixel(dc << PASS1_BITS << CONST_BITS);
        for row in 0..side as usize {
            if let Some(line) = out.get_mut(row * stride..row * stride + side as usize) {
                line.fill(value);
            }
        }
        block[0] = 0;
        return;
    }
    if side >= 8 {
        idct8(block, quant, out, stride);
    } else if side == 4 {
        idct4(block, quant, out, stride);
    } else if side == 2 {
        idct2(block, quant, out, stride);
    } else if let Some(first) = out.first_mut() {
        *first = pixel(dequantize(block, quant, 0) << PASS1_BITS << CONST_BITS);
    }
    block.fill(0);
}

#[inline(always)]
fn dequantize(block: &[i32; 64], quant: &[u16; 64], at: usize) -> W {
    Wrapping(block[at]) * Wrapping(i32::from(quant[at]))
}

/// `value >> shift`, rounded.
#[inline(always)]
fn descale(value: W, shift: usize) -> W {
    (value + Wrapping(1 << (shift - 1))) >> shift
}

/// A pass 2 output as a pixel: level-shifted and clamped.
#[inline(always)]
fn pixel(value: W) -> u8 {
    descale(value, PASS2_SHIFT)
        .0
        .wrapping_add(128)
        .clamp(0, 255) as u8
}

/// One 8-point IDCT (libjpeg's `jpeg_idct_islow`), outputs times `2√2 ·
/// 2^13`.
#[inline(always)]
fn idct8_1d(x: [W; 8]) -> [W; 8] {
    let c = Wrapping;
    // Even part: x0, x2, x4, x6.
    let z1 = (x[2] + x[6]) * c(FIX_0_541196100);
    let tmp2 = z1 + x[6] * c(-FIX_1_847759065);
    let tmp3 = z1 + x[2] * c(FIX_0_765366865);
    let tmp0 = (x[0] + x[4]) << CONST_BITS;
    let tmp1 = (x[0] - x[4]) << CONST_BITS;
    let tmp10 = tmp0 + tmp3;
    let tmp13 = tmp0 - tmp3;
    let tmp11 = tmp1 + tmp2;
    let tmp12 = tmp1 - tmp2;
    // Odd part: x7, x5, x3, x1.
    let (t0, t1, t2, t3) = (x[7], x[5], x[3], x[1]);
    let z1 = t0 + t3;
    let z2 = t1 + t2;
    let z3 = t0 + t2;
    let z4 = t1 + t3;
    let z5 = (z3 + z4) * c(FIX_1_175875602);
    let t0 = t0 * c(FIX_0_298631336);
    let t1 = t1 * c(FIX_2_053119869);
    let t2 = t2 * c(FIX_3_072711026);
    let t3 = t3 * c(FIX_1_501321110);
    let z1 = z1 * c(-FIX_0_899976223);
    let z2 = z2 * c(-FIX_2_562915447);
    let z3 = z3 * c(-FIX_1_961570560) + z5;
    let z4 = z4 * c(-FIX_0_390180644) + z5;
    let t0 = t0 + z1 + z3;
    let t1 = t1 + z2 + z4;
    let t2 = t2 + z2 + z3;
    let t3 = t3 + z1 + z4;
    [
        tmp10 + t3,
        tmp11 + t2,
        tmp12 + t1,
        tmp13 + t0,
        tmp13 - t0,
        tmp12 - t1,
        tmp11 - t2,
        tmp10 - t3,
    ]
}

/// Means of pairs: four outputs from every coefficient but `x4`.
#[inline(always)]
fn idct4_1d(x: [W; 8]) -> [W; 4] {
    let c = Wrapping;
    let dc = x[0] << CONST_BITS;
    let even = x[2] * c(PAIR_C2) - x[6] * c(PAIR_S2);
    let (e0, e1) = (dc + even, dc - even);
    let o0 = x[1] * c(PAIR_A1) - x[7] * c(PAIR_A7) + x[3] * c(PAIR_A3) - x[5] * c(PAIR_A5);
    let o1 = x[1] * c(PAIR_B1) - x[7] * c(PAIR_B7) - x[3] * c(PAIR_B3) + x[5] * c(PAIR_B5);
    [e0 + o0, e1 + o1, e1 - o1, e0 - o0]
}

/// Means of fours: two outputs from DC and the odd coefficients.
#[inline(always)]
fn idct2_1d(x0: W, x1: W, x3: W, x5: W, x7: W) -> [W; 2] {
    let c = Wrapping;
    let dc = x0 << CONST_BITS;
    let odd = x1 * c(QUAD_1) - x3 * c(QUAD_3) + x5 * c(QUAD_5) - x7 * c(QUAD_7);
    [dc + odd, dc - odd]
}

fn idct8(block: &mut [i32; 64], quant: &[u16; 64], out: &mut [u8], stride: usize) {
    for col in 0..8 {
        let ac_zero = (1..8).all(|row| block[row * 8 + col] == 0);
        if ac_zero {
            // Only DC: the column is flat.
            let dc = dequantize(block, quant, col) << PASS1_BITS;
            for row in 0..8 {
                block[row * 8 + col] = dc.0;
            }
            continue;
        }
        let x: [W; 8] = core::array::from_fn(|row| dequantize(block, quant, row * 8 + col));
        for (row, value) in idct8_1d(x).into_iter().enumerate() {
            block[row * 8 + col] = descale(value, PASS1_SHIFT).0;
        }
    }
    for row in 0..8 {
        let x: [W; 8] = core::array::from_fn(|col| Wrapping(block[row * 8 + col]));
        let pixels = idct8_1d(x);
        if let Some(line) = out.get_mut(row * stride..row * stride + 8) {
            for (slot, value) in line.iter_mut().zip(pixels) {
                *slot = pixel(value);
            }
        }
    }
}

fn idct4(block: &mut [i32; 64], quant: &[u16; 64], out: &mut [u8], stride: usize) {
    for col in 0..8 {
        if col == 4 {
            // The row pass never reads column 4.
            continue;
        }
        let x: [W; 8] = core::array::from_fn(|row| dequantize(block, quant, row * 8 + col));
        for (row, value) in idct4_1d(x).into_iter().enumerate() {
            block[row * 8 + col] = descale(value, PASS1_SHIFT).0;
        }
    }
    for row in 0..4 {
        let x: [W; 8] = core::array::from_fn(|col| Wrapping(block[row * 8 + col]));
        let pixels = idct4_1d(x);
        if let Some(line) = out.get_mut(row * stride..row * stride + 4) {
            for (slot, value) in line.iter_mut().zip(pixels) {
                *slot = pixel(value);
            }
        }
    }
}

fn idct2(block: &mut [i32; 64], quant: &[u16; 64], out: &mut [u8], stride: usize) {
    for col in [0, 1, 3, 5, 7] {
        let x = |row: usize| dequantize(block, quant, row * 8 + col);
        let values = idct2_1d(x(0), x(1), x(3), x(5), x(7));
        for (row, value) in values.into_iter().enumerate() {
            block[row * 8 + col] = descale(value, PASS1_SHIFT).0;
        }
    }
    for row in 0..2 {
        let x = |col: usize| Wrapping(block[row * 8 + col]);
        let pixels = idct2_1d(x(0), x(1), x(3), x(5), x(7));
        if let Some(line) = out.get_mut(row * stride..row * stride + 2) {
            for (slot, value) in line.iter_mut().zip(pixels) {
                *slot = pixel(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec::Vec;

    /// The JPEG IDCT in floating point, before rounding and clamping.
    fn reference(coefficients: &[f64; 64]) -> [f64; 64] {
        let c = |u: usize| {
            if u == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            }
        };
        let basis =
            |x: usize, u: usize| (((2 * x + 1) * u) as f64 * core::f64::consts::PI / 16.0).cos();
        core::array::from_fn(|at| {
            let (y, x) = (at / 8, at % 8);
            let mut sum = 0.0;
            for v in 0..8 {
                for u in 0..8 {
                    sum += c(u) * c(v) * coefficients[v * 8 + u] * basis(x, u) * basis(y, v);
                }
            }
            sum / 4.0
        })
    }

    /// The reference's means over squares of `8 / side`, level-shifted,
    /// rounded and clamped.
    fn reference_scaled(coefficients: &[f64; 64], side: usize) -> Vec<u8> {
        let full = reference(coefficients);
        let cell = 8 / side;
        let mut out = Vec::new();
        for y in 0..side {
            for x in 0..side {
                let mut sum = 0.0;
                for dy in 0..cell {
                    for dx in 0..cell {
                        sum += full[(y * cell + dy) * 8 + x * cell + dx];
                    }
                }
                let mean = sum / (cell * cell) as f64 + 128.0;
                out.push(mean.round().clamp(0.0, 255.0) as u8);
            }
        }
        out
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn range(&mut self, limit: i32) -> i32 {
            (self.next() % (2 * limit as u64 + 1)) as i32 - limit
        }
    }

    /// Blocks like a photo's after quantization: DC anywhere, AC shrinking
    /// with frequency, many zero.
    fn random_block(rng: &mut Rng) -> ([i32; 64], [u16; 64]) {
        let mut block = [0i32; 64];
        let quant: [u16; 64] = core::array::from_fn(|at| 1 + (at as u16 % 8) * 3);
        block[0] = rng.range(1000 / i32::from(quant[0]));
        for at in 1..64 {
            let (v, u) = (at / 8, at % 8);
            if rng.next() % 3 == 0 {
                let limit = 300 / (1 + (u + v) as i32) / i32::from(quant[at]);
                block[at] = rng.range(limit.max(1));
            }
        }
        (block, quant)
    }

    #[test]
    fn every_size_matches_the_mean_of_the_float_idct() {
        let mut rng = Rng(7);
        for side in [1usize, 2, 4, 8] {
            let mut worst = 0;
            for _ in 0..4000 {
                let (block, quant) = random_block(&mut rng);
                let real: [f64; 64] =
                    core::array::from_fn(|at| f64::from(block[at]) * f64::from(quant[at]));
                let expected = reference_scaled(&real, side);
                let mut ours = [0u8; 64];
                let mut work = block;
                let ac = block[1..].iter().any(|&value| value != 0);
                transform(&mut work, &quant, ac, side as u32, &mut ours, side);
                assert_eq!(work, [0; 64], "the block is left clear");
                for (at, &want) in expected.iter().enumerate() {
                    let got = ours[(at / side) * side + at % side];
                    worst = worst.max((i32::from(got) - i32::from(want)).abs());
                }
                // The general transforms agree with the flat shortcut.
                if !ac {
                    let mut general = [0u8; 64];
                    let mut work = block;
                    transform(&mut work, &quant, true, side as u32, &mut general, side);
                    assert_eq!(general[..side * side], ours[..side * side]);
                }
            }
            assert!(worst <= 1, "side {side}: off by {worst}");
        }
    }

    #[test]
    fn hostile_blocks_never_panic() {
        let mut rng = Rng(8);
        for _ in 0..20_000 {
            let block: [i32; 64] = core::array::from_fn(|_| rng.range(32768));
            let quant: [u16; 64] = core::array::from_fn(|_| rng.next() as u16);
            for side in [1, 2, 4, 8] {
                let mut work = block;
                let mut out = [0u8; 64];
                transform(&mut work, &quant, true, side, &mut out, side as usize);
            }
        }
    }
}
