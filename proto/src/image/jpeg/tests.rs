//! The JPEG decoder against zune-jpeg, on files jpeg-encoder writes, and
//! against an exact reference, on files the small encoder here writes for
//! the layouts jpeg-encoder cannot: odd sampling, several scans, SOF1,
//! 16-bit tables, Adobe and RGB markers, five Huffman tables.

extern crate std;

use super::*;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::vec;
use std::vec::Vec;

/// SplitMix64, so the tests need no PRNG dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// A photo-like plane: soft waves and rings with a little noise, kept
/// between 16 and 240 so that clamping never decides a result.
fn plane(width: usize, height: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let (fx, fy) = (7.0 + (seed % 11) as f64, 13.0 + (seed % 7) as f64);
    let (cx, cy) = (width as f64 * 0.4, height as f64 * 0.6);
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (xf, yf) = (x as f64, y as f64);
            let wave = 45.0 * (xf / fx + yf / fy).sin();
            let ring = 35.0 * (((xf - cx).hypot(yf - cy)) / 6.0).cos();
            let edge = if (x / 23 + y / 17) % 2 == 0 {
                12.0
            } else {
                -12.0
            };
            let noise = (rng.below(9) as f64) - 4.0;
            out.push((128.0 + wave + ring + edge + noise).clamp(16.0, 240.0) as u8);
        }
    }
    out
}

/// Interleaves three planes as YCbCr pixels for jpeg-encoder.
fn ycbcr(width: usize, height: usize, seed: u64) -> (Vec<u8>, Vec<u8>) {
    let y = plane(width, height, seed);
    let cb = plane(width, height, seed + 1);
    let cr = plane(width, height, seed + 2);
    let pixels = (0..width * height)
        .flat_map(|at| [y[at], cb[at], cr[at]])
        .collect();
    (pixels, y)
}

#[derive(Clone, Copy, Debug)]
struct Settings {
    color: bool,
    sampling: SamplingFactor,
    quality: u8,
    restart: Option<u16>,
    optimized: bool,
    progressive: bool,
}

const PLAIN: Settings = Settings {
    color: true,
    sampling: SamplingFactor::F_2_2,
    quality: 85,
    restart: None,
    optimized: false,
    progressive: false,
};

/// jpeg-encoder's file for a test image of `width × height`.
fn encode(width: usize, height: usize, seed: u64, settings: Settings) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, settings.quality);
    encoder.set_sampling_factor(settings.sampling);
    if let Some(interval) = settings.restart {
        encoder.set_restart_interval(interval);
    }
    encoder.set_optimized_huffman_tables(settings.optimized);
    encoder.set_progressive(settings.progressive);
    let (w, h) = (width as u16, height as u16);
    if settings.color {
        let (pixels, _) = ycbcr(width, height, seed);
        encoder.encode(&pixels, w, h, ColorType::Ycbcr).unwrap();
    } else {
        encoder
            .encode(&plane(width, height, seed), w, h, ColorType::Luma)
            .unwrap();
    }
    out
}

/// zune-jpeg's luma: Y for a YCbCr file, the plane for a gray one.
fn zune_luma(file: &[u8]) -> (usize, usize, Vec<u8>) {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(file), options);
    let luma = decoder.decode().expect("zune decodes it");
    let info = decoder.info().unwrap();
    (usize::from(info.width), usize::from(info.height), luma)
}

/// The box filter written the obvious way.
fn box_filter(full: &[u8], width: usize, height: usize, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut sum = vec![0u64; out_w * out_h];
    let mut count = vec![0u64; out_w * out_h];
    for y in 0..height {
        for x in 0..width {
            let at = (y * out_h / height) * out_w + x * out_w / width;
            sum[at] += u64::from(full[y * width + x]);
            count[at] += 1;
        }
    }
    sum.iter()
        .zip(&count)
        .map(|(&s, &c)| ((s + c / 2) / c) as u8)
        .collect()
}

struct Collect {
    width: usize,
    rows: u32,
    luma: Vec<u8>,
}

impl LumaSink for Collect {
    fn row(&mut self, y: u32, luma: &[u8]) -> Result<(), SinkError> {
        assert_eq!(y, self.rows, "rows arrive in order");
        assert_eq!(luma.len(), self.width, "rows are exactly the output width");
        self.rows += 1;
        self.luma.extend_from_slice(luma);
        Ok(())
    }
}

/// Feeds `file` in pieces whose sizes `piece` picks from the bytes left.
fn decode_in(
    file: &[u8],
    (out_w, out_h): (u32, u32),
    scratch: &mut [u8],
    mut piece: impl FnMut(usize) -> usize,
) -> Result<Vec<u8>, JpegError> {
    let mut sink = Collect {
        width: out_w as usize,
        rows: 0,
        luma: Vec::new(),
    };
    let mut decoder = JpegDecoder::new(scratch, out_w, out_h)?;
    let mut rest = file;
    while !rest.is_empty() {
        let n = piece(rest.len()).clamp(1, rest.len());
        let (part, tail) = rest.split_at(n);
        decoder.feed(part, &mut sink)?;
        rest = tail;
    }
    decoder.finish()?;
    assert_eq!(sink.rows, out_h, "a decode that succeeds emits every row");
    assert!(
        decoder.blocks <= 8 * file.len() as u64 + MAX_UNITS as u64,
        "{} blocks from {} bytes",
        decoder.blocks,
        file.len()
    );
    Ok(sink.luma)
}

/// Decodes whole, in exactly the scratch the probe asks for, once over
/// each of three junk fills, which must all agree.
fn decode(file: &[u8], out: (u32, u32)) -> Result<Vec<u8>, JpegError> {
    let info = probe(file)?;
    let mut scratch = vec![0xA5; info.scratch_bytes(out.0, out.1)];
    let result = decode_in(file, out, &mut scratch, |n| n);
    for junk in [0x00, 0xFF] {
        scratch.fill(junk);
        let again = decode_in(file, out, &mut scratch, |n| n);
        assert_eq!(again, result, "scratch filled with {junk:#04x}");
    }
    result
}

/// The mean and largest difference between two images.
fn difference(ours: &[u8], reference: &[u8]) -> (f64, u8) {
    assert_eq!(ours.len(), reference.len());
    let mut total = 0u64;
    let mut worst = 0u8;
    for (&a, &b) in ours.iter().zip(reference) {
        let d = a.abs_diff(b);
        total += u64::from(d);
        worst = worst.max(d);
    }
    (total as f64 / ours.len().max(1) as f64, worst)
}

/// 4:4:4, 4:2:2, 4:2:0 and 4:4:0.
const SAMPLINGS: [SamplingFactor; 4] = [
    SamplingFactor::F_1_1,
    SamplingFactor::F_2_1,
    SamplingFactor::F_2_2,
    SamplingFactor::F_1_2,
];

const SIZES: [(usize, usize); 10] = [
    (1, 1),
    (7, 9),
    (8, 8),
    (16, 16),
    (17, 33),
    (33, 17),
    (63, 65),
    (100, 75),
    (161, 97),
    (250, 180),
];

/// Every jpeg-encoder layout written as one interleaved scan, at full
/// size, against zune-jpeg. Both use an integer IDCT of the islow kind, so
/// they agree to a level almost everywhere; the limit allows for the two
/// rounding differently. (Files with a scan per component are checked
/// against their interleaved twins instead: zune-jpeg 0.5.15 decodes their
/// subsampled luma wrongly, past its first row of blocks.)
#[test]
fn full_size_matches_zune() {
    let mut cases = 0;
    let mut seed = 0;
    for color in [false, true] {
        for (index, &sampling) in SAMPLINGS.iter().enumerate() {
            if !color && index > 0 {
                continue;
            }
            for &(width, height) in &SIZES {
                for (quality, restart) in [(85, None), (50, Some(3)), (97, Some(1)), (20, None)] {
                    seed += 1;
                    let settings = Settings {
                        color,
                        sampling,
                        quality,
                        restart,
                        ..PLAIN
                    };
                    let file = encode(width, height, seed, settings);
                    let (w, h, reference) = zune_luma(&file);
                    assert_eq!((w, h), (width, height));
                    let ours = decode(&file, (width as u32, height as u32)).unwrap();
                    let (mean, worst) = difference(&ours, &reference);
                    assert!(
                        mean <= 0.25 && worst <= 2,
                        "{settings:?} {width}x{height}: mean {mean:.3}, worst {worst}"
                    );
                    cases += 1;
                }
            }
        }
    }
    assert!(cases > 150, "{cases} cases");
}

/// jpeg-encoder writes optimized tables as one scan per component, luma
/// first. Luma's quantized coefficients are the same either way, so the
/// decode must be too, to the byte, at every size and scale.
#[test]
fn a_scan_per_component_decodes_as_the_interleaved_file_does() {
    let mut rng = Rng(23);
    for color in [false, true] {
        for &sampling in &SAMPLINGS {
            for &(width, height) in &SIZES {
                let settings = Settings {
                    color,
                    sampling,
                    ..PLAIN
                };
                let interleaved = encode(width, height, 7, settings);
                let separate = encode(
                    width,
                    height,
                    7,
                    Settings {
                        optimized: true,
                        ..settings
                    },
                );
                if color {
                    let scans = (0..separate.len() - 1)
                        .filter(|&at| separate[at] == 0xFF && separate[at + 1] == SOS)
                        .count();
                    assert_eq!(scans, 3);
                }
                for out in [
                    (width as u32, height as u32),
                    (1 + rng.below(width) as u32, 1 + rng.below(height) as u32),
                ] {
                    assert_eq!(
                        decode(&separate, out).unwrap(),
                        decode(&interleaved, out).unwrap(),
                        "{settings:?} {width}x{height} to {out:?}"
                    );
                }
            }
        }
    }
}

/// Means over squares of `cell` pixels, the last ones in each row and
/// column cut short by the image's edge: the full-size image as the
/// reduced IDCTs see it.
fn squares(full: &[u8], width: usize, height: usize, cell: usize) -> (usize, usize, Vec<u8>) {
    let (scaled_w, scaled_h) = (width.div_ceil(cell), height.div_ceil(cell));
    let mut out = Vec::with_capacity(scaled_w * scaled_h);
    for y in 0..scaled_h {
        for x in 0..scaled_w {
            let (mut sum, mut count) = (0u32, 0u32);
            for yy in y * cell..((y + 1) * cell).min(height) {
                for xx in x * cell..((x + 1) * cell).min(width) {
                    sum += u32::from(full[yy * width + xx]);
                    count += 1;
                }
            }
            out.push(((sum + count / 2) / count) as u8);
        }
    }
    (scaled_w, scaled_h, out)
}

/// The reference for a decode to `out`: the full-size luma averaged over
/// the squares the decode's scale gives, then box-filtered to `out`, as
/// the decoder does.
fn scaled_reference(
    full: &[u8],
    width: usize,
    height: usize,
    side: u32,
    out: (u32, u32),
) -> Vec<u8> {
    let (scaled_w, scaled_h, scaled) = squares(full, width, height, 8 / side as usize);
    box_filter(&scaled, scaled_w, scaled_h, out.0 as usize, out.1 as usize)
}

/// Downscaled decodes against zune's full-size luma, averaged over the
/// squares of the decode's scale and box-filtered to the same size. The
/// reduced IDCTs give exactly those squares' means up to rounding, and
/// the filter's mean of row means rounds twice.
#[test]
fn every_scale_matches_the_reference() {
    let mut rng = Rng(3);
    let mut by_side = [0usize; 9];
    let mut seed = 100;
    for &sampling in &SAMPLINGS {
        for color in [false, true] {
            // Sides a multiple of 8, so no square holds the encoder's
            // padding, which zune does not show (the exact reference below
            // covers that).
            for &(width, height) in &[(248usize, 184usize), (160, 96), (336, 208), (64, 64)] {
                seed += 1;
                let settings = Settings {
                    color,
                    sampling,
                    ..PLAIN
                };
                let file = encode(width, height, seed, settings);
                let (_, _, full) = zune_luma(&file);
                let info = probe(&file).unwrap();
                let mut outs: Vec<(u32, u32)> = (0..6)
                    .map(|_| (1 + rng.below(width) as u32, 1 + rng.below(height) as u32))
                    .collect();
                for side in [1u32, 2, 4] {
                    outs.push((
                        (width as u32 * side).div_ceil(8),
                        (height as u32 * side).div_ceil(8),
                    ));
                }
                for out in outs {
                    let side = info.block_side(out.0, out.1);
                    by_side[side as usize] += 1;
                    let ours = decode(&file, out).unwrap();
                    let reference = scaled_reference(&full, width, height, side, out);
                    let (mean, worst) = difference(&ours, &reference);
                    assert!(
                        mean <= 0.5 && worst <= 2,
                        "{settings:?} {width}x{height} to {out:?} at side {side}: \
                         mean {mean:.3}, worst {worst}"
                    );
                }
            }
        }
    }
    for side in [1, 2, 4, 8] {
        assert!(
            by_side[side] >= 10,
            "side {side}: {} decodes",
            by_side[side]
        );
    }
}

/// A smooth photo-like plane: broad waves and a soft vignette, the
/// content a downscale meets most.
fn smooth_plane(width: usize, height: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed);
    let (fx, fy) = (31.0 + (seed % 13) as f64, 43.0 + (seed % 7) as f64);
    let (cx, cy) = (width as f64 * 0.5, height as f64 * 0.45);
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        for x in 0..width {
            let (xf, yf) = (x as f64, y as f64);
            let wave = 40.0 * (xf / fx).sin() * (yf / fy).cos();
            let vignette = 30.0 * ((xf - cx).hypot(yf - cy) / (width.max(height) as f64)).cos();
            let noise = (rng.below(5) as f64) - 2.0;
            out.push((110.0 + wave + vignette + noise).clamp(16.0, 240.0) as u8);
        }
    }
    out
}

/// The same downscales against zune's full-size luma box-filtered
/// straight to the output size. The decoder filters the scaled image, so
/// where an output column's span does not line up with the 2, 4 or 8
/// pixel squares, a square that straddles two output pixels counts whole
/// in one: on smooth content that moves a pixel by a few levels at most.
#[test]
fn downscales_match_the_full_size_reference_box_filtered() {
    let mut rng = Rng(24);
    let mut worst_seen = (0.0f64, 0u8);
    for (seed, &(width, height)) in [(400usize, 300usize), (333, 211), (250, 180), (601, 451)]
        .iter()
        .enumerate()
    {
        let pixels = smooth_plane(width, height, seed as u64);
        let mut file = Vec::new();
        Encoder::new(&mut file, 90)
            .encode(&pixels, width as u16, height as u16, ColorType::Luma)
            .unwrap();
        let (_, _, full) = zune_luma(&file);
        for _ in 0..12 {
            let out = (1 + rng.below(width) as u32, 1 + rng.below(height) as u32);
            let ours = decode(&file, out).unwrap();
            let reference = box_filter(&full, width, height, out.0 as usize, out.1 as usize);
            let (mean, worst) = difference(&ours, &reference);
            worst_seen = (worst_seen.0.max(mean), worst_seen.1.max(worst));
            assert!(
                mean <= 2.0 && worst <= 8,
                "{width}x{height} to {out:?}: mean {mean:.3}, worst {worst}"
            );
        }
    }
    std::println!(
        "worst mean {:.3}, worst pixel {}",
        worst_seen.0,
        worst_seen.1
    );
}

#[test]
fn the_scale_is_the_smallest_that_still_covers_the_output() {
    let info = |width, height| JpegInfo {
        width,
        height,
        coding: 0xC0,
        precision: 8,
        components: 3,
        luma: (2, 2),
        max: (2, 2),
        factors_ok: true,
        rgb: false,
    };
    // 1600 × 1200 into the X4's landscape box (601 × 451): half size is
    // 800 × 600, a quarter 400 × 300 is too small.
    assert_eq!(info(1600, 1200).block_side(601, 451), 4);
    assert_eq!(info(1600, 1200).block_side(200, 150), 1);
    assert_eq!(info(1600, 1200).block_side(201, 150), 2);
    assert_eq!(info(1600, 1200).block_side(1600, 1200), 8);
    // Rounded up, as libjpeg does: 17 at 1/8 is 3.
    assert_eq!(info(17, 17).block_side(3, 3), 1);
    assert_eq!(info(17, 17).block_side(4, 3), 2);
    // Either side can decide.
    assert_eq!(info(1600, 100).block_side(100, 100), 8);
    // A 4,000-px-wide 4:2:0 photo fitted to 784 decodes at 1/4.
    let photo = info(4000, 3000);
    assert_eq!(photo.block_side(784, 588), 2);
    assert_eq!(photo.scratch_bytes(784, 588), 10_499);
}

/// Every supported layout at every output width a page box has, and the
/// worst case of each, fits the scratch the firmware lends.
#[test]
fn scratch_stays_inside_20_kb_at_784_wide() {
    let mut worst = 0;
    for components in [1u8, 3] {
        for luma in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            for width in [785u32, 1000, 1567, 1568, 3135, 4000, 16_384] {
                let info = JpegInfo {
                    width,
                    height: width,
                    coding: 0xC0,
                    precision: 8,
                    components,
                    luma,
                    max: (2, 2),
                    factors_ok: true,
                    rgb: false,
                };
                for out_w in [1, 64, 464, 512, 776, 784] {
                    let bytes = info.scratch_bytes(out_w, out_w);
                    assert!(bytes <= FIXED_SCRATCH + 20 * out_w as usize);
                    worst = worst.max(bytes);
                }
            }
        }
    }
    assert_eq!(worst, 19_907);
    assert_eq!(FIXED_SCRATCH, 4227);
}

#[test]
fn feeding_in_any_pieces_gives_the_same_rows() {
    let mut rng = Rng(4);
    for (sampling, restart) in [
        (SamplingFactor::F_2_2, None),
        (SamplingFactor::F_2_1, Some(2)),
        (SamplingFactor::F_1_1, Some(1)),
    ] {
        let file = encode(
            61,
            43,
            9,
            Settings {
                sampling,
                restart,
                ..PLAIN
            },
        );
        for out in [(61, 43), (30, 21), (8, 6), (3, 2)] {
            let whole = decode(&file, out).unwrap();
            let info = probe(&file).unwrap();
            let mut scratch = vec![0xA5; info.scratch_bytes(out.0, out.1)];
            let bytes = decode_in(&file, out, &mut scratch, |_| 1).unwrap();
            assert_eq!(bytes, whole, "a byte at a time, {out:?}");
            for _ in 0..20 {
                let mut sizes = Rng(rng.next());
                let random =
                    decode_in(&file, out, &mut scratch, |n| 1 + sizes.below(n.min(700))).unwrap();
                assert_eq!(random, whole, "random pieces, {out:?}");
            }
        }
    }
}

#[test]
fn one_scratch_serves_consecutive_images() {
    let mut rng = Rng(5);
    let mut scratch = vec![0xA5u8; 64 * 1024];
    for round in 0..40 {
        let width = 1 + rng.below(90);
        let height = 1 + rng.below(70);
        let settings = Settings {
            color: round % 3 != 0,
            sampling: SAMPLINGS[round % SAMPLINGS.len()],
            quality: 40 + rng.below(60) as u8,
            ..PLAIN
        };
        let file = encode(width, height, round as u64, settings);
        let out = (1 + rng.below(width) as u32, 1 + rng.below(height) as u32);
        let fresh = decode(&file, out).unwrap();
        // Whatever the last image left behind stays in the scratch.
        let reused = decode_in(&file, out, &mut scratch, |n| n).unwrap();
        assert_eq!(reused, fresh);
    }
}

#[test]
fn a_sink_error_stops_the_decode_for_good() {
    let file = encode(16, 64, 12, PLAIN);
    let mut scratch = vec![0xA5; 32 * 1024];
    let mut seen = 0;
    let mut sink = |y: u32, _: &[u8]| {
        seen += 1;
        if y == 2 {
            Err(SinkError::Cancelled)
        } else {
            Ok(())
        }
    };
    let mut decoder = JpegDecoder::new(&mut scratch, 16, 64).unwrap();
    let stopped = Err(JpegError::Sink(SinkError::Cancelled));
    assert_eq!(decoder.feed(&file, &mut sink), stopped);
    assert_eq!(decoder.feed(&file, &mut sink), stopped);
    assert_eq!(decoder.finish(), stopped);
    assert_eq!(seen, 3);
}

#[test]
fn a_decode_needs_every_row_and_nothing_after() {
    let file = encode(40, 40, 13, PLAIN);
    let full = decode(&file, (40, 40)).unwrap();
    // Anything after the last row is never read, however damaged.
    let mut padded = file.clone();
    padded.truncate(padded.len() - 2);
    padded.extend((0..5000).map(|i| (i * 7) as u8));
    assert_eq!(decode(&padded, (40, 40)).unwrap(), full);
    // A file cut inside its scan is short.
    for cut in [file.len() - 40, file.len() / 2, 300, 2] {
        let short = &file[..cut];
        let mut scratch = vec![0xA5; 32 * 1024];
        assert!(
            matches!(
                decode_in(short, (40, 40), &mut scratch, |n| n),
                Err(JpegError::Truncated)
            ),
            "cut at {cut}"
        );
    }
    // EOI where the data should go on.
    let header_end = scan_data_start(&file);
    let mut early = file[..header_end + 20].to_vec();
    early.extend_from_slice(&[0xFF, 0xD9]);
    let mut scratch = vec![0xA5; 32 * 1024];
    assert!(decode_in(&early, (40, 40), &mut scratch, |n| n).is_err());
}

/// Where the first scan's entropy-coded data starts.
fn scan_data_start(file: &[u8]) -> usize {
    let mut at = 2;
    loop {
        assert_eq!(file[at], 0xFF);
        let code = file[at + 1];
        let len = usize::from(u16::from_be_bytes([file[at + 2], file[at + 3]]));
        if code == SOS {
            return at + 2 + len;
        }
        at += 2 + len;
    }
}

/// A tiny file claiming a huge image must fail after a few MCUs, not
/// decode millions of blocks of zeros.
#[test]
fn a_huge_frame_with_little_data_fails_fast() {
    let file = encode(
        64,
        64,
        14,
        Settings {
            color: false,
            ..PLAIN
        },
    );
    let mut huge = file.clone();
    // SOF0's height and width: 16,384 × 4,096.
    let sof = (0..huge.len())
        .find(|&at| huge[at] == 0xFF && huge[at + 1] == 0xC0)
        .unwrap();
    huge[sof + 5..sof + 9].copy_from_slice(&[0x10, 0x00, 0x40, 0x00]);
    let mut scratch = vec![0xA5; 32 * 1024];
    let mut decoder = JpegDecoder::new(&mut scratch, 64, 16).unwrap();
    let mut sink = |_: u32, _: &[u8]| Ok(());
    let result = decoder
        .feed(&huge, &mut sink)
        .and_then(|()| decoder.finish());
    assert!(result.is_err());
    assert!(decoder.blocks <= 8 * huge.len() as u64 + MAX_UNITS as u64);
}

#[test]
fn progressive_arithmetic_lossless_and_cmyk_are_unsupported() {
    let progressive = encode(
        32,
        32,
        15,
        Settings {
            progressive: true,
            ..PLAIN
        },
    );
    let info = probe(&progressive).unwrap();
    assert_eq!(info.coding, 0xC2);
    assert_eq!((info.width, info.height), (32, 32));
    assert_eq!(info.check(), Err(JpegError::Unsupported));
    assert_eq!(decode_err(&progressive), JpegError::Unsupported);

    // The same file's SOF0 relabelled: arithmetic (SOF9), lossless (SOF3),
    // differential (SOF5), and 12-bit samples.
    let baseline = encode(32, 32, 16, PLAIN);
    let sof = (0..baseline.len())
        .find(|&at| baseline[at] == 0xFF && baseline[at + 1] == 0xC0)
        .unwrap();
    for code in [0xC9, 0xC3, 0xC5, 0xCA, 0xCB, 0xCD] {
        let mut file = baseline.clone();
        file[sof + 1] = code;
        assert_eq!(probe(&file).unwrap().check(), Err(JpegError::Unsupported));
        assert_eq!(decode_err(&file), JpegError::Unsupported, "{code:#x}");
    }
    let mut twelve = baseline.clone();
    twelve[sof + 4] = 12;
    assert_eq!(decode_err(&twelve), JpegError::Unsupported);
    // SOF1, extended sequential, decodes as baseline does.
    let mut extended = baseline.clone();
    extended[sof + 1] = 0xC1;
    assert_eq!(
        decode(&extended, (32, 32)).unwrap(),
        decode(&baseline, (32, 32)).unwrap()
    );

    let mut cmyk = Vec::new();
    let pixels: Vec<u8> = (0..32 * 32 * 4).map(|i| (i * 13) as u8).collect();
    Encoder::new(&mut cmyk, 80)
        .encode(&pixels, 32, 32, ColorType::Cmyk)
        .unwrap();
    assert_eq!(probe(&cmyk).unwrap().components, 4);
    assert_eq!(decode_err(&cmyk), JpegError::Unsupported);
}

fn decode_err(file: &[u8]) -> JpegError {
    let mut scratch = vec![0xA5; 32 * 1024];
    decode_in(file, (8, 8), &mut scratch, |n| n).unwrap_err()
}

#[test]
fn probe_reads_past_large_segments_and_says_what_it_may_skip() {
    let file = encode(48, 40, 17, PLAIN);
    // A 64 KB APP1, as an EXIF thumbnail can be, before everything else.
    let mut big = file[..2].to_vec();
    let body: Vec<u8> = (0..65_533u32).map(|i| (i % 251) as u8).collect();
    big.extend_from_slice(&[0xFF, 0xE1]);
    big.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
    big.extend_from_slice(&body);
    big.extend_from_slice(&file[2..]);
    let info = probe(&big).unwrap();
    assert_eq!((info.width, info.height), (48, 40));
    assert_eq!(
        decode(&big, (48, 40)).unwrap(),
        decode(&file, (48, 40)).unwrap()
    );

    // Fed the first bytes, the probe asks to pass over the APP1 body.
    let mut probe = JpegProbe::new();
    assert_eq!(probe.feed(&big[..10]), None);
    assert_eq!(probe.skippable(), 65_533 - 4);
    probe.skip(65_533 - 4);
    assert_eq!(probe.skippable(), 0);
    let rest = &big[6 + 65_533..];
    assert_eq!(probe.feed(rest), Some(Ok(info)));

    // A prefix that stops short of the frame header is truncated.
    assert_eq!(super::probe(&big[..4000]), Err(JpegError::Truncated));
    assert_eq!(super::probe(b"\x89PNG\r\n"), Err(JpegError::NotJpeg));
    assert_eq!(super::probe(&[]), Err(JpegError::Truncated));
}

#[test]
fn output_size_must_be_within_the_image() {
    let file = encode(20, 10, 18, PLAIN);
    let mut scratch = vec![0xA5; 32 * 1024];
    assert!(matches!(
        JpegDecoder::new(&mut scratch, 0, 5),
        Err(JpegError::BadOutputSize)
    ));
    for out in [(21, 10), (20, 11)] {
        assert_eq!(
            decode_in(&file, out, &mut scratch, |n| n),
            Err(JpegError::BadOutputSize)
        );
    }
    let info = probe(&file).unwrap();
    let need = info.scratch_bytes(20, 10);
    for short in [need - 1, FIXED_SCRATCH - 1, 10] {
        // Pointer alignment can lend up to three bytes of slack.
        let mut small = vec![0xA5; short];
        let result = decode_in(&file, (20, 10), &mut small, |n| n);
        if short < need - ALIGN_SLACK {
            assert_eq!(result, Err(JpegError::ScratchTooSmall), "{short}");
        }
    }
}

#[test]
fn decoder_state_stays_small() {
    // Locals and the decoder itself must stay well inside the X3's few KB
    // of spare stack.
    assert!(
        core::mem::size_of::<JpegDecoder>() <= 320,
        "{}",
        core::mem::size_of::<JpegDecoder>()
    );
    assert!(core::mem::size_of::<JpegProbe>() <= 96);
    std::println!(
        "decoder {} probe {}",
        core::mem::size_of::<JpegDecoder>(),
        core::mem::size_of::<JpegProbe>()
    );
}

// ---------------------------------------------------------------------
// A small baseline encoder, for the layouts jpeg-encoder does not write.

/// One component of a file [`write`] makes.
#[derive(Clone, Debug)]
struct Comp {
    id: u8,
    across: u8,
    down: u8,
    quant: u8,
    dc: u8,
    ac: u8,
}

#[derive(Clone, Debug)]
struct Spec {
    width: usize,
    height: usize,
    coding: u8,
    comps: Vec<Comp>,
    /// Each scan's components, by index.
    scans: Vec<Vec<usize>>,
    restart: u16,
    /// Quantization tables by id: 16-bit entries, values in natural order.
    quant: Vec<(u8, bool, [u16; 64])>,
    jfif: bool,
    adobe: Option<u8>,
    seed: u64,
}

fn spec(factors: &[(u8, u8)]) -> Spec {
    let comps = factors
        .iter()
        .enumerate()
        .map(|(index, &(across, down))| Comp {
            id: index as u8 + 1,
            across,
            down,
            quant: u8::from(index > 0),
            dc: u8::from(index > 0),
            ac: u8::from(index > 0),
        })
        .collect::<Vec<_>>();
    let flat: [u16; 64] = core::array::from_fn(|at| 2 + (at as u16 % 8) + (at as u16 / 8));
    let coarse: [u16; 64] = core::array::from_fn(|at| 6 + 2 * (at as u16 % 8 + at as u16 / 8));
    Spec {
        width: 50,
        height: 38,
        coding: 0xC0,
        scans: vec![(0..comps.len()).collect()],
        comps,
        restart: 0,
        quant: vec![(0, false, flat), (1, false, coarse)],
        jfif: true,
        adobe: None,
        seed: 1,
    }
}

/// The Huffman tables jpeg-encoder writes (the standard ones): DC then AC,
/// luma then chroma, as `(counts, symbols)`.
fn standard_tables() -> [(Vec<u8>, Vec<u8>); 4] {
    let file = encode(16, 16, 1, PLAIN);
    let mut tables: [(Vec<u8>, Vec<u8>); 4] = Default::default();
    let mut at = 2;
    while at + 4 <= file.len() && file[at] == 0xFF && file[at + 1] != SOS {
        let len = usize::from(u16::from_be_bytes([file[at + 2], file[at + 3]]));
        if file[at + 1] == DHT {
            let mut body = &file[at + 4..at + 2 + len];
            while !body.is_empty() {
                let (class, id) = (body[0] >> 4, body[0] & 15);
                let counts = body[1..17].to_vec();
                let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
                let symbols = body[17..17 + total].to_vec();
                tables[usize::from(class * 2 + id)] = (counts, symbols);
                body = &body[17 + total..];
            }
        }
        at += 2 + len;
    }
    tables
}

/// Code and length of every symbol of a table.
fn codes(counts: &[u8], symbols: &[u8]) -> [(u32, u32); 256] {
    let mut out = [(0, 0); 256];
    let (mut code, mut index) = (0u32, 0usize);
    for (len, &count) in (1..=16).zip(counts) {
        for _ in 0..count {
            out[usize::from(symbols[index])] = (code, len);
            code += 1;
            index += 1;
        }
        code <<= 1;
    }
    out
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, len: u32) {
        for bit in (0..len).rev() {
            self.acc = self.acc << 1 | (value >> bit & 1);
            self.bits += 1;
            if self.bits == 8 {
                let byte = self.acc as u8;
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0);
                }
                self.acc = 0;
                self.bits = 0;
            }
        }
    }

    /// Pads the last byte with ones.
    fn flush(&mut self) {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.put((1 << pad) - 1, pad);
        }
    }
}

/// The forward DCT of a block, quantized.
fn quantize_block(pixels: &[f64; 64], quant: &[u16; 64]) -> [i32; 64] {
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
        let (v, u) = (at / 8, at % 8);
        let mut sum = 0.0;
        for y in 0..8 {
            for x in 0..8 {
                sum += (pixels[y * 8 + x] - 128.0) * basis(x, u) * basis(y, v);
            }
        }
        let coefficient = sum * c(u) * c(v) / 4.0;
        (coefficient / f64::from(quant[at])).round() as i32
    })
}

/// The float IDCT of a dequantized block, rounded and clamped.
fn reconstruct(coefficients: &[i32; 64], quant: &[u16; 64]) -> [u8; 64] {
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
                let value = f64::from(coefficients[v * 8 + u]) * f64::from(quant[v * 8 + u]);
                sum += c(u) * c(v) * value * basis(x, u) * basis(y, v);
            }
        }
        (sum / 4.0 + 128.0).round().clamp(0.0, 255.0) as u8
    })
}

/// A file [`write_file`] made, with what it must decode to.
struct Written {
    file: Vec<u8>,
    /// The float IDCT of luma's quantized blocks at the image's size, each
    /// pixel repeated where luma is sampled more coarsely than the image.
    luma: Vec<u8>,
    /// The same over every block of luma's plane, padding included, its
    /// size in pixels, and the shifts from image to luma pixels.
    padded: Vec<u8>,
    padded_w: usize,
    plane_size: (usize, usize),
    shift: (usize, usize),
}

/// Writes `spec` as a file, and returns it with the luma it must decode
/// to.
fn write(spec: &Spec) -> (Vec<u8>, Vec<u8>) {
    let written = write_file(spec);
    (written.file, written.luma)
}

/// What a decode of `written` to `out` at `side` must give: luma's padded
/// plane averaged over whole squares, as the reduced IDCTs average their
/// blocks, cut to luma's scaled size, each pixel repeated where luma is
/// coarser than the image, cut to the scaled image, and box-filtered.
fn exact_scaled(
    written: &Written,
    width: usize,
    height: usize,
    side: u32,
    out: (u32, u32),
) -> Vec<u8> {
    let side = side as usize;
    let cell = 8 / side;
    let padded_h = written.padded.len() / written.padded_w;
    let (squares_w, _, squared) = squares(&written.padded, written.padded_w, padded_h, cell);
    let (plane_w, plane_h) = written.plane_size;
    let (luma_w, luma_h) = ((plane_w * side).div_ceil(8), (plane_h * side).div_ceil(8));
    let (scaled_w, scaled_h) = ((width * side).div_ceil(8), (height * side).div_ceil(8));
    let mut scaled = Vec::with_capacity(scaled_w * scaled_h);
    for y in 0..scaled_h {
        for x in 0..scaled_w {
            let (lx, ly) = (x >> written.shift.0, y >> written.shift.1);
            assert!(lx < luma_w && ly < luma_h);
            scaled.push(squared[ly * squares_w + lx]);
        }
    }
    box_filter(&scaled, scaled_w, scaled_h, out.0 as usize, out.1 as usize)
}

fn write_file(spec: &Spec) -> Written {
    let (width, height) = (spec.width, spec.height);
    let max_x = spec
        .comps
        .iter()
        .map(|c| usize::from(c.across))
        .max()
        .unwrap();
    let max_y = spec
        .comps
        .iter()
        .map(|c| usize::from(c.down))
        .max()
        .unwrap();
    let mcus_x = width.div_ceil(8 * max_x);
    let mcus_y = height.div_ceil(8 * max_y);
    let quant_of = |id: u8| {
        spec.quant
            .iter()
            .find(|table| table.0 == id)
            .map(|table| table.2)
            .unwrap()
    };
    // Each component's samples and quantized blocks, on a grid padded to
    // whole MCUs.
    let mut blocks: Vec<(usize, usize, Vec<[i32; 64]>)> = Vec::new();
    let mut luma = Vec::new();
    let mut padded = Vec::new();
    let (mut padded_w, mut plane_size, mut shift) = (0, (0, 0), (0, 0));
    for (index, comp) in spec.comps.iter().enumerate() {
        let comp_w = (width * usize::from(comp.across)).div_ceil(max_x);
        let comp_h = (height * usize::from(comp.down)).div_ceil(max_y);
        let samples = plane(comp_w, comp_h, spec.seed + index as u64);
        let grid_w = mcus_x * usize::from(comp.across);
        let grid_h = mcus_y * usize::from(comp.down);
        let quant = quant_of(comp.quant);
        let mut grid = Vec::new();
        for by in 0..grid_h {
            for bx in 0..grid_w {
                let pixels: [f64; 64] = core::array::from_fn(|at| {
                    let x = (bx * 8 + at % 8).min(comp_w - 1);
                    let y = (by * 8 + at / 8).min(comp_h - 1);
                    f64::from(samples[y * comp_w + x])
                });
                grid.push(quantize_block(&pixels, &quant));
            }
        }
        if index == 0 {
            let (blocks_w, blocks_h) = (comp_w.div_ceil(8), comp_h.div_ceil(8));
            padded_w = blocks_w * 8;
            padded = vec![0u8; padded_w * blocks_h * 8];
            for by in 0..blocks_h {
                for bx in 0..blocks_w {
                    let pixels = reconstruct(&grid[by * grid_w + bx], &quant);
                    for at in 0..64 {
                        let (x, y) = (bx * 8 + at % 8, by * 8 + at / 8);
                        padded[y * padded_w + x] = pixels[at];
                    }
                }
            }
            shift = (
                usize::from(max_x > usize::from(comp.across)),
                usize::from(max_y > usize::from(comp.down)),
            );
            plane_size = (comp_w, comp_h);
            for y in 0..height {
                for x in 0..width {
                    luma.push(padded[(y >> shift.1) * padded_w + (x >> shift.0)]);
                }
            }
        }
        blocks.push((grid_w, comp_h, grid));
    }

    let mut file = vec![0xFF, SOI];
    let segment = |file: &mut Vec<u8>, code: u8, body: &[u8]| {
        file.extend_from_slice(&[0xFF, code]);
        file.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
        file.extend_from_slice(body);
    };
    if spec.jfif {
        segment(&mut file, APP0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    }
    if let Some(transform) = spec.adobe {
        let mut body = b"Adobe\0\x64\0\0\0\0".to_vec();
        body.push(transform);
        segment(&mut file, APP14, &body);
    }
    for (id, wide, table) in &spec.quant {
        let mut body = vec![u8::from(*wide) << 4 | id];
        for &natural in &ZIGZAG {
            let value = table[usize::from(natural)];
            if *wide {
                body.extend_from_slice(&value.to_be_bytes());
            } else {
                body.push(value as u8);
            }
        }
        segment(&mut file, DQT, &body);
    }
    let mut sof = vec![8];
    sof.extend_from_slice(&(height as u16).to_be_bytes());
    sof.extend_from_slice(&(width as u16).to_be_bytes());
    sof.push(spec.comps.len() as u8);
    for comp in &spec.comps {
        sof.extend_from_slice(&[comp.id, comp.across << 4 | comp.down, comp.quant]);
    }
    segment(&mut file, spec.coding, &sof);
    let standard = standard_tables();
    let mut defined = Vec::new();
    for comp in &spec.comps {
        for (class, id) in [(0u8, comp.dc), (1, comp.ac)] {
            if defined.contains(&(class, id)) {
                continue;
            }
            defined.push((class, id));
            // Ids 0 and 2 get luma's tables, 1 and 3 chroma's.
            let (counts, symbols) = &standard[usize::from(class * 2 + (id & 1))];
            let mut body = vec![class << 4 | id];
            body.extend_from_slice(counts);
            body.extend_from_slice(symbols);
            segment(&mut file, DHT, &body);
        }
    }
    if spec.restart > 0 {
        segment(&mut file, DRI, &spec.restart.to_be_bytes());
    }
    for members in &spec.scans {
        let mut sos = vec![members.len() as u8];
        for &index in members {
            let comp = &spec.comps[index];
            sos.extend_from_slice(&[comp.id, comp.dc << 4 | comp.ac]);
        }
        sos.extend_from_slice(&[0, 63, 0]);
        segment(&mut file, SOS, &sos);
        // The MCUs of this scan: (component, block column, block row).
        let mut mcus: Vec<Vec<(usize, usize, usize)>> = Vec::new();
        if members.len() > 1 {
            for my in 0..mcus_y {
                for mx in 0..mcus_x {
                    let mut units = Vec::new();
                    for &index in members {
                        let comp = &spec.comps[index];
                        for dy in 0..usize::from(comp.down) {
                            for dx in 0..usize::from(comp.across) {
                                units.push((
                                    index,
                                    mx * usize::from(comp.across) + dx,
                                    my * usize::from(comp.down) + dy,
                                ));
                            }
                        }
                    }
                    mcus.push(units);
                }
            }
        } else {
            let index = members[0];
            let comp = &spec.comps[index];
            let comp_w = (width * usize::from(comp.across)).div_ceil(max_x);
            let comp_h = blocks[index].1;
            for by in 0..comp_h.div_ceil(8) {
                for bx in 0..comp_w.div_ceil(8) {
                    mcus.push(vec![(index, bx, by)]);
                }
            }
        }
        let mut bits = BitWriter {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        };
        let mut preds = vec![0i32; spec.comps.len()];
        let mut marker = 0u8;
        for (number, units) in mcus.iter().enumerate() {
            if spec.restart > 0 && number > 0 && number % usize::from(spec.restart) == 0 {
                bits.flush();
                bits.out.extend_from_slice(&[0xFF, 0xD0 + marker]);
                marker = (marker + 1) & 7;
                preds.fill(0);
            }
            for &(index, bx, by) in units {
                let comp = &spec.comps[index];
                let (grid_w, _, grid) = &blocks[index];
                let block = &grid[by * grid_w + bx];
                let dc = codes(
                    &standard[usize::from(comp.dc & 1)].0,
                    &standard[usize::from(comp.dc & 1)].1,
                );
                let ac = codes(
                    &standard[usize::from(2 + (comp.ac & 1))].0,
                    &standard[usize::from(2 + (comp.ac & 1))].1,
                );
                put_block(&mut bits, block, &mut preds[index], &dc, &ac);
            }
        }
        bits.flush();
        file.extend_from_slice(&bits.out);
    }
    file.extend_from_slice(&[0xFF, EOI]);
    Written {
        file,
        luma,
        padded,
        padded_w,
        plane_size,
        shift,
    }
}

fn put_block(
    bits: &mut BitWriter,
    block: &[i32; 64],
    pred: &mut i32,
    dc: &[(u32, u32); 256],
    ac: &[(u32, u32); 256],
) {
    let category = |value: i32| 32 - value.unsigned_abs().leading_zeros();
    let extra = |value: i32, size: u32| {
        if value < 0 {
            (value + (1 << size) - 1) as u32
        } else {
            value as u32
        }
    };
    let diff = block[0] - *pred;
    *pred = block[0];
    let size = category(diff);
    let (code, len) = dc[size as usize];
    bits.put(code, len);
    bits.put(extra(diff, size), size);
    let mut run = 0;
    for k in 1..64 {
        let value = block[usize::from(ZIGZAG[k])];
        if value == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            let (code, len) = ac[0xF0];
            bits.put(code, len);
            run -= 16;
        }
        let size = category(value);
        let (code, len) = ac[(run << 4 | size) as usize];
        bits.put(code, len);
        bits.put(extra(value, size), size);
        run = 0;
    }
    if run > 0 {
        let (code, len) = ac[0];
        bits.put(code, len);
    }
}

/// zune-jpeg agrees with the small encoder's own reference on the layout
/// both know, so the encoder is right; the decoder then must match the
/// reference to a level on every layout.
#[test]
fn the_small_encoder_agrees_with_zune() {
    for factors in [
        &[(2, 2), (1, 1), (1, 1)][..],
        &[(1, 1)],
        &[(2, 1), (1, 1), (1, 1)],
    ] {
        let (file, luma) = write(&spec(factors));
        let (_, _, reference) = zune_luma(&file);
        let (mean, worst) = difference(&luma, &reference);
        assert!(mean <= 0.25 && worst <= 2, "{factors:?}: {mean:.3} {worst}");
    }
}

/// Odd samplings, luma coarser than chroma included, at full size and at
/// each scale, against the exact reference. Where luma is coarser, a
/// downscale averages luma's own pixels, each standing for two of the
/// image's, so the reference does too.
#[test]
fn odd_samplings_match_the_exact_reference() {
    let layouts: [&[(u8, u8)]; 9] = [
        &[(1, 1)],
        &[(2, 2)],
        &[(1, 2), (1, 1), (1, 1)],
        &[(1, 2), (2, 1), (1, 1)],
        &[(2, 1), (1, 2), (1, 1)],
        &[(1, 1), (2, 2), (2, 2)],
        &[(1, 1), (2, 1), (1, 2)],
        &[(2, 2), (2, 2), (2, 2)],
        &[(2, 2), (1, 2), (2, 1)],
    ];
    let mut rng = Rng(19);
    for (seed, &factors) in layouts.iter().enumerate() {
        for (width, height) in [(50, 38), (17, 9), (1, 1), (64, 48), (33, 70)] {
            let mut spec = spec(factors);
            (spec.width, spec.height, spec.seed) = (width, height, seed as u64 * 10);
            let written = write_file(&spec);
            let file = &written.file;
            let full = decode(file, (width as u32, height as u32)).unwrap();
            let (mean, worst) = difference(&full, &written.luma);
            assert!(
                mean <= 0.25 && worst <= 1,
                "{factors:?} {width}x{height}: mean {mean:.3}, worst {worst}"
            );
            let info = probe(file).unwrap();
            let mut outs: Vec<(u32, u32)> = (0..6)
                .map(|_| (1 + rng.below(width) as u32, 1 + rng.below(height) as u32))
                .collect();
            for side in [1u32, 2, 4] {
                let w = (width as u32 * side).div_ceil(8);
                let h = (height as u32 * side).div_ceil(8);
                if info.block_side(w, h) == side {
                    outs.push((w, h));
                }
            }
            for out in outs {
                let side = info.block_side(out.0, out.1);
                let ours = decode(file, out).unwrap();
                let reference = exact_scaled(&written, width, height, side, out);
                let (mean, worst) = difference(&ours, &reference);
                // A few pixels, each a level off from rounding twice, can
                // make a mean of more than half a level.
                assert!(
                    mean <= 0.75 && worst <= 2,
                    "{factors:?} {width}x{height} to {out:?} at side {side}: \
                     mean {mean:.3}, worst {worst}"
                );
            }
        }
    }
}

/// Scans one component at a time, in any order, luma first or last, and
/// restart intervals in each kind of scan.
#[test]
fn separate_scans_and_restart_intervals() {
    let orders: [&[&[usize]]; 5] = [
        &[&[0], &[1], &[2]],
        &[&[1], &[2], &[0]],
        &[&[1, 2], &[0]],
        &[&[0, 1], &[2]],
        &[&[2], &[0, 1]],
    ];
    for (seed, scans) in orders.iter().enumerate() {
        for restart in [0u16, 1, 3, 7] {
            for factors in [&[(2, 2), (1, 1), (1, 1)][..], &[(1, 2), (2, 1), (1, 1)]] {
                let mut spec = spec(factors);
                spec.scans = scans.iter().map(|scan| scan.to_vec()).collect();
                spec.restart = restart;
                spec.seed = seed as u64;
                let (file, luma) = write(&spec);
                let out = (spec.width as u32, spec.height as u32);
                let ours = decode(&file, out).unwrap();
                let (mean, worst) = difference(&ours, &luma);
                assert!(
                    mean <= 0.25 && worst <= 1,
                    "{scans:?} restart {restart}: mean {mean:.3}, worst {worst}"
                );
                // Fed a byte at a time, through restart markers and scan
                // boundaries.
                let mut scratch = vec![0xA5; 32 * 1024];
                assert_eq!(decode_in(&file, out, &mut scratch, |_| 1).unwrap(), ours);
                let half = decode(&file, (out.0 / 2, out.1 / 2)).unwrap();
                assert_eq!(half.len(), (out.0 / 2 * (out.1 / 2)) as usize);
            }
        }
    }
}

/// SOF1 with 16-bit quantization entries and Huffman tables 2 and 3.
#[test]
fn extended_sequential_with_wide_tables_and_high_ids() {
    let mut spec = spec(&[(2, 2), (1, 1), (1, 1)]);
    spec.coding = 0xC1;
    let wide: [u16; 64] = core::array::from_fn(|at| 3 + at as u16);
    spec.quant = vec![(2, true, wide), (3, true, wide)];
    for comp in &mut spec.comps {
        comp.quant += 2;
        comp.dc += 2;
        comp.ac += 2;
    }
    let (file, luma) = write(&spec);
    let ours = decode(&file, (50, 38)).unwrap();
    let (mean, worst) = difference(&ours, &luma);
    assert!(mean <= 0.25 && worst <= 1, "mean {mean:.3}, worst {worst}");

    // Five tables at once is one more than the decoder keeps.
    let mut five = spec.clone();
    five.comps[1].dc = 0;
    let (file, _) = write(&five);
    assert_eq!(decode_err(&file), JpegError::Unsupported);
}

#[test]
fn rgb_is_unsupported_and_ycbcr_markers_are_honoured() {
    let mut adobe = spec(&[(1, 1), (1, 1), (1, 1)]);
    adobe.jfif = false;
    adobe.adobe = Some(0);
    let (file, _) = write(&adobe);
    assert!(probe(&file).unwrap().rgb);
    assert_eq!(decode_err(&file), JpegError::Unsupported);
    // Transform 1 is YCbCr.
    adobe.adobe = Some(1);
    let (file, luma) = write(&adobe);
    assert_eq!(decode(&file, (50, 38)).unwrap().len(), luma.len());

    // Components named R, G, B are RGB without JFIF, YCbCr with it.
    let mut named = spec(&[(1, 1), (1, 1), (1, 1)]);
    for (comp, id) in named.comps.iter_mut().zip(*b"RGB") {
        comp.id = id;
    }
    named.jfif = false;
    let (file, _) = write(&named);
    assert_eq!(decode_err(&file), JpegError::Unsupported);
    named.jfif = true;
    let (file, _) = write(&named);
    assert!(decode(&file, (50, 38)).is_ok());

    // An Adobe marker after the frame header still counts.
    let mut late = spec(&[(1, 1), (1, 1), (1, 1)]);
    late.jfif = false;
    let (file, _) = write(&late);
    let sos = (0..file.len())
        .find(|&at| file[at] == 0xFF && file[at + 1] == SOS)
        .unwrap();
    let mut moved = file[..sos].to_vec();
    moved.extend_from_slice(&[0xFF, APP14, 0, 14]);
    moved.extend_from_slice(b"Adobe\0\x64\0\0\0\0\0");
    moved.extend_from_slice(&file[sos..]);
    assert!(!probe(&moved).unwrap().rgb, "the probe stops at the frame");
    assert_eq!(decode_err(&moved), JpegError::Unsupported);
}

/// Sampling factors of 3 and 4 are valid JPEG the decoder does not take.
#[test]
fn factors_past_two_are_unsupported() {
    let file = encode(
        64,
        32,
        20,
        Settings {
            sampling: SamplingFactor::F_4_1,
            ..PLAIN
        },
    );
    let info = probe(&file).unwrap();
    assert!(!info.factors_ok);
    assert_eq!(info.check(), Err(JpegError::Unsupported));
    // A lone component's factors do not matter.
    let mut gray = spec(&[(4, 4)]);
    gray.width = 20;
    let (file, luma) = write(&gray);
    let (mean, worst) = difference(&decode(&file, (20, 38)).unwrap(), &luma);
    assert!(mean <= 0.25 && worst <= 1, "mean {mean:.3}, worst {worst}");
}

#[test]
fn damaged_and_random_input_never_panics() {
    let mut rng = Rng(21);
    let mut corpus = Vec::new();
    for round in 0..24u64 {
        // jpeg-encoder's optimizer cannot take a restart interval.
        let restart = [None, Some(1), Some(4)][round as usize % 3];
        let settings = Settings {
            color: round % 4 != 0,
            sampling: SAMPLINGS[round as usize % SAMPLINGS.len()],
            quality: 30 + (round * 7 % 70) as u8,
            restart,
            optimized: restart.is_none() && round % 2 == 0,
            progressive: round % 11 == 5,
        };
        let width = 1 + rng.below(70);
        let height = 1 + rng.below(50);
        corpus.push(encode(width, height, round, settings));
    }
    for (index, factors) in [
        &[(1, 2), (2, 1), (1, 1)][..],
        &[(1, 1), (2, 2), (2, 2)],
        &[(2, 2), (1, 1), (1, 1)],
    ]
    .iter()
    .enumerate()
    {
        let mut spec = spec(factors);
        spec.restart = index as u16 * 2;
        if index == 2 {
            spec.scans = vec![vec![1], vec![2], vec![0]];
        }
        corpus.push(write(&spec).0);
    }
    let (mut ok, mut failed) = (0, 0);
    for _ in 0..20_000 {
        let mut file = corpus[rng.below(corpus.len())].clone();
        match rng.below(7) {
            0 => {
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(file.len());
                    file[at] = rng.next() as u8;
                }
            }
            1 => {
                // Damage past the headers, where the scan data is.
                let start = scan_data_start(&file).min(file.len() - 1);
                for _ in 0..1 + rng.below(8) {
                    let at = start + rng.below(file.len() - start);
                    file[at] ^= 1 << rng.below(8);
                }
            }
            2 => file.truncate(rng.below(file.len())),
            3 => {
                let other = &corpus[rng.below(corpus.len())];
                let cut = rng.below(file.len());
                file.truncate(cut);
                file.extend_from_slice(&other[rng.below(other.len())..]);
            }
            4 => {
                // A segment length set to anything.
                let mut at = 2;
                let mut starts = Vec::new();
                while at + 4 <= file.len() && file[at] == 0xFF && file[at + 1] != SOS {
                    starts.push(at + 2);
                    at += 2 + usize::from(u16::from_be_bytes([file[at + 2], file[at + 3]]));
                }
                if let Some(&at) = starts.get(rng.below(starts.len())) {
                    let len = match rng.below(3) {
                        0 => rng.next() as u16,
                        1 => rng.below(64) as u16,
                        _ => 0,
                    };
                    file[at..at + 2].copy_from_slice(&len.to_be_bytes());
                }
            }
            5 => {
                let at = rng.below(file.len());
                if rng.below(2) == 0 {
                    let count = 1 + rng.below(16);
                    file.splice(at..at, (0..count).map(|_| rng.next() as u8));
                } else {
                    let end = (at + 1 + rng.below(16)).min(file.len());
                    file.drain(at..end);
                }
            }
            _ => {
                // Noise behind real headers.
                let start = scan_data_start(&file).min(file.len());
                file.truncate(start);
                file.extend((0..rng.below(2000)).map(|_| rng.next() as u8));
            }
        }
        let (out, scratch_len) = match probe(&file) {
            Ok(info) if info.is_supported() => {
                let out = (
                    1 + rng.below(info.width.min(300) as usize) as u32,
                    1 + rng.below(info.height.min(48) as usize) as u32,
                );
                (out, info.scratch_bytes(out.0, out.1).min(1 << 18))
            }
            _ => (
                (1 + rng.below(4) as u32, 1 + rng.below(4) as u32),
                rng.below(8192),
            ),
        };
        let mut scratch = vec![0xA5; scratch_len];
        let mut sizes = Rng(rng.next());
        match decode_in(&file, out, &mut scratch, |n| 1 + sizes.below(n.min(97))) {
            Ok(luma) => {
                assert_eq!(luma.len(), (out.0 * out.1) as usize);
                ok += 1;
            }
            Err(_) => failed += 1,
        }
    }
    // Both paths ran plenty.
    assert!(ok > 1000 && failed > 5000, "{ok} decoded, {failed} failed");
}

/// Random bytes after a valid SOI, and random tables and headers.
#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(22);
    for _ in 0..5000 {
        let mut file = vec![0xFF, SOI];
        for _ in 0..rng.below(6) {
            // Plausible segments with random bodies.
            let code = [DQT, DHT, 0xC0, 0xC1, SOS, DRI, APP0, APP14, 0xFE][rng.below(9)];
            let len = rng.below(80);
            file.extend_from_slice(&[0xFF, code]);
            file.extend_from_slice(&(len as u16 + 2).to_be_bytes());
            file.extend((0..len).map(|_| rng.next() as u8));
        }
        file.extend((0..rng.below(300)).map(|_| rng.next() as u8));
        let mut scratch = vec![0xA5; rng.below(16 * 1024)];
        let out = (1 + rng.below(8) as u32, 1 + rng.below(8) as u32);
        let mut sizes = Rng(rng.next());
        let _ = decode_in(&file, out, &mut scratch, |n| 1 + sizes.below(n.min(31)));
        let _ = probe(&file);
    }
}

#[test]
fn huffman_tables_must_fit_their_lengths() {
    let mut slot = vec![0u8; SLOT_BYTES];
    // Three codes of length 1 cannot exist.
    slot[SLOT_COUNTS] = 3;
    assert_eq!(build_table(&mut slot, false), Err(JpegError::Corrupt));
    // Two of length 1 are the whole space; a DC size of 16 is too big.
    slot[SLOT_COUNTS] = 2;
    slot[SLOT_VALUES] = 3;
    slot[SLOT_VALUES + 1] = 16;
    assert_eq!(build_table(&mut slot, true), Err(JpegError::Corrupt));
    assert_eq!(build_table(&mut slot, false), Ok(()));
    let fast: &[u16] = bytemuck::cast_slice(&slot[..SLOT_COUNTS]);
    assert_eq!(fast[0], 0x0103);
    assert_eq!(fast[255], 0x0110);
    // Every symbol of the standard tables is found, by the lookup or the
    // walk.
    for (class, (counts, symbols)) in standard_tables().iter().enumerate() {
        let mut slot = vec![0u8; SLOT_BYTES];
        slot[SLOT_COUNTS..SLOT_VALUES].copy_from_slice(counts);
        slot[SLOT_VALUES..SLOT_VALUES + symbols.len()].copy_from_slice(symbols);
        build_table(&mut slot, class < 2).unwrap();
        let table = Huffman::of(&slot, 0).unwrap();
        for (symbol, &(code, len)) in codes(counts, symbols).iter().enumerate() {
            if len == 0 {
                continue;
            }
            let mut bytes = Vec::new();
            for byte in (code << (32 - len)).to_be_bytes() {
                bytes.push(byte);
                if byte == 0xFF {
                    bytes.push(0);
                }
            }
            let mut bits = Bits {
                state: BitState::default(),
                input: &bytes,
                pos: 0,
            };
            assert_eq!(bits.decode(&table), Ok(Some(symbol as u8)));
            assert_eq!(bits.state.have, 32 - len);
        }
    }
}

/// Black ink on white paper, as Gutenberg's engravings are: lines and
/// hatching at full black on full white, the content that rings hardest.
fn line_art(width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![255u8; width * height];
    for y in 0..height {
        for x in 0..width {
            let ring =
                ((x as f64 - width as f64 / 2.0).hypot(y as f64 - height as f64 / 2.0)) as usize;
            let hatch = (x + y) % 7 == 0 && x < width / 2;
            let line = x % 23 == 0 || y % 31 == 0 || ring % 19 == 0;
            if hatch || line {
                out[y * width + x] = 0;
            }
        }
    }
    out
}

/// Saturated line art at reduced scales. The reduced IDCTs clamp the mean
/// of each square, not each pixel. Beside white paper JPEG rings past
/// 255, beside black ink below 0; clamping each pixel first keeps the
/// ringing on the side it does not cut, so the full-size decode averaged
/// over the squares is grayer at both ends than the drawing was. Against
/// that reference this drawing measures (mean, worst) 2.8/7, 2.7/10 and
/// 2.9/19 levels at 1/8, 1/4 and 1/2 at quality 60; 0.9/3, 1.0/4 and
/// 1.1/7 at 85; within 2 at 95. Against the drawing itself the decoder is
/// the closer of the two at every scale and quality (at 60 and 1/2, 1.8
/// mean against 3.4), which the test checks, with those gaps as bounds.
#[test]
fn saturated_line_art_at_reduced_scales_stays_nearer_the_drawing() {
    let (width, height) = (248usize, 184usize);
    let pixels = line_art(width, height);
    for (quality, mean_limit, worst_limit) in [(60u8, 3.0, 20u8), (85, 1.2, 8), (95, 0.5, 2)] {
        let mut file = Vec::new();
        Encoder::new(&mut file, quality)
            .encode(&pixels, width as u16, height as u16, ColorType::Luma)
            .unwrap();
        let (_, _, full) = zune_luma(&file);
        for side in [1u32, 2, 4] {
            let out = (width as u32 * side / 8, height as u32 * side / 8);
            assert_eq!(probe(&file).unwrap().block_side(out.0, out.1), side);
            let ours = decode(&file, out).unwrap();
            let reference = scaled_reference(&full, width, height, side, out);
            let drawing = scaled_reference(&pixels, width, height, side, out);
            let (mean, worst) = difference(&ours, &reference);
            assert!(
                mean <= mean_limit && worst <= worst_limit,
                "q{quality} side {side}: mean {mean:.3}, worst {worst}"
            );
            let (ours_off, _) = difference(&ours, &drawing);
            let (reference_off, _) = difference(&reference, &drawing);
            assert!(
                ours_off <= reference_off,
                "q{quality} side {side}: {ours_off:.3} from the drawing, the reference {reference_off:.3}"
            );
        }
    }
}

/// Everything before the first scan's data.
fn headers(file: &[u8]) -> &[u8] {
    &file[..scan_data_start(file)]
}

/// Feeds `file` in 4 KB pieces until the decoder fails, returning the
/// error and how many bytes it was given.
fn feed_until_error(file: &[u8], out: (u32, u32)) -> (Result<(), JpegError>, usize) {
    let mut scratch = vec![0xA5; 64 * 1024];
    let mut decoder = JpegDecoder::new(&mut scratch, out.0, out.1).unwrap();
    let mut sink = |_: u32, _: &[u8]| Ok(());
    let mut fed = 0;
    for piece in file.chunks(4096) {
        fed += piece.len();
        if let Err(error) = decoder.feed(piece, &mut sink) {
            assert_eq!(
                decoder.feed(piece, &mut sink),
                Err(error),
                "it stays failed"
            );
            assert_eq!(decoder.finish(), Err(error));
            return (Err(error), fed);
        }
    }
    (decoder.finish(), fed)
}

#[test]
fn an_eoi_with_rows_to_come_fails_at_once_whatever_follows() {
    let file = encode(64, 48, 25, PLAIN);
    // EOI before any scan, then megabytes of junk: the first piece fails.
    let mut early = headers(&file).to_vec();
    early.extend_from_slice(&[0xFF, EOI]);
    early.extend((0..8u32 << 20).map(|i| (i * 13) as u8));
    assert_eq!(
        feed_until_error(&early, (64, 48)),
        (Err(JpegError::Truncated), 4096)
    );
    // EOI part way through the scan.
    let mut cut = file[..scan_data_start(&file) + 100].to_vec();
    cut.extend_from_slice(&[0xFF, EOI]);
    cut.extend(std::iter::repeat_n(0u8, 1 << 20));
    assert_eq!(
        feed_until_error(&cut, (64, 48)),
        (Err(JpegError::Truncated), 4096)
    );
    // After the last row, anything may follow.
    let mut whole = file.clone();
    whole.extend(std::iter::repeat_n(0u8, 100_000));
    assert_eq!(feed_until_error(&whole, (64, 48)).0, Ok(()));
}

#[test]
fn bytes_outside_segments_are_bounded() {
    let file = encode(64, 48, 26, PLAIN);
    let full = decode(&file, (64, 48)).unwrap();
    let sof = (0..file.len())
        .find(|&at| file[at] == 0xFF && file[at + 1] == 0xC0)
        .unwrap();
    let with = |junk: &[u8]| {
        let mut out = file[..sof].to_vec();
        out.extend_from_slice(junk);
        out.extend_from_slice(&file[sof..]);
        out
    };
    // A few stray bytes and fill before a marker are passed over.
    let few = with(&[0, 0, 0x55, 0xFF, 0xFF]);
    assert_eq!(decode(&few, (64, 48)).unwrap(), full);
    let stray: Vec<u8> = vec![0; usize::from(MAX_STRAY)];
    assert_eq!(decode(&with(&stray), (64, 48)).unwrap(), full);
    // Past the bound, the file is corrupt, however much more follows.
    let mut junk = vec![0x11u8; 5 << 20];
    junk.extend_from_slice(&file[sof..]);
    let mut endless = file[..sof].to_vec();
    endless.extend_from_slice(&junk);
    let (result, fed) = feed_until_error(&endless, (64, 48));
    assert_eq!((result, fed <= sof + 4096), (Err(JpegError::Corrupt), true));
    // So are stray TEM and restart markers, which have no segment.
    let markers: Vec<u8> = std::iter::repeat_n([0xFF, 0x01, 0xFF, 0xD3], 1 << 16)
        .flatten()
        .collect();
    let (result, fed) = feed_until_error(&with(&markers), (64, 48));
    assert_eq!((result, fed <= sof + 4096), (Err(JpegError::Corrupt), true));
}

/// A gray file of 8 × 8 MCUs with a restart marker after every one.
fn restarted() -> (Vec<u8>, Vec<u8>) {
    let mut spec = spec(&[(1, 1)]);
    (spec.width, spec.height, spec.restart) = (40, 16, 1);
    write(&spec)
}

/// Where each restart marker in the scan sits.
fn restart_markers(file: &[u8]) -> Vec<usize> {
    let start = scan_data_start(file);
    (start..file.len() - 1)
        .filter(|&at| file[at] == 0xFF && file[at + 1] & 0xF8 == 0xD0)
        .collect()
}

#[test]
fn restart_markers_must_come_in_sequence_where_they_are_due() {
    let (file, luma) = restarted();
    let markers = restart_markers(&file);
    assert_eq!(markers.len(), 9, "ten MCUs, a marker between each");
    let decoded = decode(&file, (40, 16)).unwrap();
    assert!(difference(&decoded, &luma).1 <= 1);
    for (index, &at) in markers.iter().enumerate() {
        assert_eq!(file[at + 1], 0xD0 + (index % 8) as u8);
    }
    // Fill before a marker is fine.
    let mut filled = file.clone();
    filled.splice(markers[3]..markers[3], [0xFF, 0xFF]);
    assert_eq!(decode(&filled, (40, 16)).unwrap(), decoded);
    // A wrong number, another marker, a missing marker, or data before
    // one, is corrupt.
    let mut wrong = file.clone();
    wrong[markers[2] + 1] = 0xD5;
    let mut other = file.clone();
    other[markers[2] + 1] = 0xFE;
    let mut missing = file.clone();
    missing.drain(markers[4]..markers[4] + 2);
    let mut extra = file.clone();
    extra.insert(markers[5], 0x2A);
    for (name, damaged) in [
        ("wrong", wrong),
        ("other", other),
        ("missing", missing),
        ("extra", extra),
    ] {
        let mut scratch = vec![0xA5; 32 * 1024];
        assert!(
            decode_in(&damaged, (40, 16), &mut scratch, |n| n).is_err(),
            "{name}"
        );
        let mut bytes = vec![0xA5; 32 * 1024];
        assert!(
            decode_in(&damaged, (40, 16), &mut bytes, |_| 1).is_err(),
            "{name}, a byte at a time"
        );
    }
}

/// A scan of nothing but restart markers would decode every MCU from
/// made-up bits, two bytes each: an empty interval fails instead.
#[test]
fn a_scan_of_only_restart_markers_fails_at_once() {
    let (file, _) = restarted();
    let mut bomb = headers(&file).to_vec();
    // The frame claims 16,384 × 4,096, 1,048,576 MCUs.
    let sof = (0..bomb.len())
        .find(|&at| bomb[at] == 0xFF && bomb[at + 1] == 0xC0)
        .unwrap();
    bomb[sof + 5..sof + 9].copy_from_slice(&[0x10, 0x00, 0x40, 0x00]);
    for n in 0..(1u32 << 18) {
        bomb.extend_from_slice(&[0xFF, 0xD0 | (n % 8) as u8]);
    }
    let mut scratch = vec![0xA5; 32 * 1024];
    let mut decoder = JpegDecoder::new(&mut scratch, 64, 16).unwrap();
    let mut sink = |_: u32, _: &[u8]| Ok(());
    assert_eq!(decoder.feed(&bomb, &mut sink), Err(JpegError::Truncated));
    assert_eq!(decoder.blocks, 0);
    // So does one with a real first interval.
    let markers = restart_markers(&file);
    let mut late = file[..markers[0] + 2].to_vec();
    late.extend_from_slice(&[0xFF, 0xD1, 0xFF, 0xD2]);
    let mut scratch = vec![0xA5; 32 * 1024];
    let mut decoder = JpegDecoder::new(&mut scratch, 40, 16).unwrap();
    assert_eq!(decoder.feed(&late, &mut sink), Err(JpegError::Truncated));
    assert_eq!(decoder.blocks, 1);
}
