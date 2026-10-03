//! Golden-frame comparison shared by the scenario `--check` path and the
//! reading-surface tests (`tests/reading_golden.rs` includes this file).
//!
//! Frames compare by decoded pixels, not encoded PNG bytes, so a png crate
//! upgrade that changes compression or filtering does not fail every golden.
//! A mismatch writes `<name>.actual.png` (same format as the goldens, so it
//! can be copied over one to accept a change) and `<name>.diff.png` into
//! `target/golden-diff/` at the repo root, and reports how many pixels
//! differ and where.

use display::fb::Framebuffer;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const PAPER: u8 = 0xEE;
const INK: u8 = 0x18;

/// True only when `var` is exactly "1", so `VAR=0` or an empty value never
/// overwrites goldens.
#[allow(dead_code)] // Used by the reading tests, not by the binary.
pub fn regen_requested(var: &str) -> bool {
    std::env::var(var).as_deref() == Ok("1")
}

/// The frame as 8-bit gray, row-major in panel-native orientation.
pub fn gray_pixels(fb: &Framebuffer) -> Vec<u8> {
    let mut data = Vec::with_capacity(display::WIDTH * display::HEIGHT);
    for y in 0..display::HEIGHT {
        for x in 0..display::WIDTH {
            data.push(if fb.native_pixel(x, y) { PAPER } else { INK });
        }
    }
    data
}

/// Encode a frame in the golden format: 8-bit grayscale PNG at panel size.
pub fn encode_png<W: Write>(writer: W, fb: &Framebuffer) -> Result<(), png::EncodingError> {
    encode(
        writer,
        display::WIDTH as u32,
        display::HEIGHT as u32,
        png::ColorType::Grayscale,
        &gray_pixels(fb),
    )
}

/// Overwrite (or create) a golden with this frame.
#[allow(dead_code)] // Used by the reading tests, not by the binary.
pub fn write_golden(path: &Path, fb: &Framebuffer) -> Result<(), String> {
    write_png_file(path, |writer| encode_png(writer, fb))
}

/// Compare a frame against a golden PNG by decoded pixels.
pub fn check_frame(golden: &Path, fb: &Framebuffer) -> Result<(), String> {
    let (width, height) = (display::WIDTH, display::HEIGHT);
    let actual = gray_pixels(fb);
    let name = golden
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("frame")
        .to_owned();
    let dir = diff_dir();
    let actual_path = dir.join(format!("{name}.actual.png"));
    let diff_path = dir.join(format!("{name}.diff.png"));

    let mismatch = match decode_gray(golden) {
        Err(err) => format!("cannot read golden {}: {err}", golden.display()),
        Ok((w, h, _)) if (w, h) != (width, height) => format!(
            "golden {} is {w}x{h}, frame is {width}x{height}",
            golden.display()
        ),
        Ok((_, _, expected)) => match diff_pixels(&expected, &actual, width) {
            None => {
                // Drop artifacts from an earlier failing run.
                let _ = std::fs::remove_file(&actual_path);
                let _ = std::fs::remove_file(&diff_path);
                return Ok(());
            }
            Some(diff) => {
                write_png_file(&diff_path, |writer| {
                    encode(
                        writer,
                        width as u32,
                        height as u32,
                        png::ColorType::Rgb,
                        &diff_image(&expected, &actual),
                    )
                })?;
                format!(
                    "frame differs from {}: {} pixels in x {}..={}, y {}..={}; wrote {}",
                    golden.display(),
                    diff.count,
                    diff.min_x,
                    diff.max_x,
                    diff.min_y,
                    diff.max_y,
                    diff_path.display(),
                )
            }
        },
    };
    write_png_file(&actual_path, |writer| encode_png(writer, fb))?;
    Err(format!(
        "{mismatch}; actual frame {}",
        actual_path.display()
    ))
}

struct PixelDiff {
    count: usize,
    min_x: usize,
    max_x: usize,
    min_y: usize,
    max_y: usize,
}

fn diff_pixels(expected: &[u8], actual: &[u8], width: usize) -> Option<PixelDiff> {
    let mut diff: Option<PixelDiff> = None;
    for (index, (e, a)) in expected.iter().zip(actual).enumerate() {
        if e == a {
            continue;
        }
        let (x, y) = (index % width, index / width);
        let d = diff.get_or_insert(PixelDiff {
            count: 0,
            min_x: x,
            max_x: x,
            min_y: y,
            max_y: y,
        });
        d.count += 1;
        d.min_x = d.min_x.min(x);
        d.max_x = d.max_x.max(x);
        d.min_y = d.min_y.min(y);
        d.max_y = d.max_y.max(y);
    }
    diff
}

/// Unchanged pixels faded to a light wash of the frame; ink the golden lacks
/// in red, ink the frame lost in blue.
fn diff_image(expected: &[u8], actual: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(actual.len() * 3);
    for (&e, &a) in expected.iter().zip(actual) {
        let pixel = if e == a {
            let faded = 0x80 + a / 2;
            [faded, faded, faded]
        } else if a < e {
            [0xD0, 0x10, 0x10]
        } else {
            [0x10, 0x40, 0xD0]
        };
        rgb.extend_from_slice(&pixel);
    }
    rgb
}

/// Decode any 8- or 16-bit PNG to gray: `(width, height, pixels)`.
fn decode_gray(path: &Path) -> Result<(usize, usize, Vec<u8>), String> {
    let file = File::open(path).map_err(|err| err.to_string())?;
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|err| err.to_string())?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|err| err.to_string())?;
    let data = &buffer[..info.buffer_size()];
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => return Err(format!("unsupported PNG color type {other:?}")),
    };
    let gray = data
        .chunks_exact(channels)
        .map(|px| match channels {
            1 | 2 => px[0],
            _ => {
                let luma = 299 * px[0] as u32 + 587 * px[1] as u32 + 114 * px[2] as u32;
                ((luma + 500) / 1000) as u8
            }
        })
        .collect();
    Ok((info.width as usize, info.height as usize, gray))
}

fn diff_dir() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // tools/emulator -> repo root
    let root = manifest.ancestors().nth(2).unwrap_or(manifest);
    root.join("target/golden-diff")
}

fn encode<W: Write>(
    writer: W,
    width: u32,
    height: u32,
    color: png::ColorType,
    data: &[u8],
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(data)?;
    writer.finish()
}

fn write_png_file(
    path: &Path,
    encode: impl FnOnce(&mut BufWriter<File>) -> Result<(), png::EncodingError>,
) -> Result<(), String> {
    let describe = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| describe(&err))?;
    }
    let mut writer = BufWriter::new(File::create(path).map_err(|err| describe(&err))?);
    encode(&mut writer).map_err(|err| describe(&err))?;
    writer.flush().map_err(|err| describe(&err))
}
