//! JPEGs for host tests of firmware code that reads books' images, made
//! reproducibly with jpeg-encoder.
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

/// A photo-like YCbCr picture: soft waves and rings, so every scale has
/// something to average.
pub fn photo(width: u16, height: u16, progressive: bool) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for y in 0..height {
        for x in 0..width {
            let (xf, yf) = (f64::from(x), f64::from(y));
            let ring =
                ((xf - f64::from(width) / 2.0).hypot(yf - f64::from(height) / 3.0) / 9.0).cos();
            let luma = 128.0 + 70.0 * (xf / 37.0 + yf / 53.0).sin() + 40.0 * ring;
            pixels.push(luma.clamp(0.0, 255.0) as u8);
            pixels.push((128.0 + 40.0 * (xf / 50.0).cos()) as u8);
            pixels.push((128.0 + 40.0 * (yf / 40.0).sin()) as u8);
        }
    }
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, 85);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder.set_progressive(progressive);
    encoder
        .encode(&pixels, width, height, ColorType::Ycbcr)
        .expect("encodes");
    out
}

/// `jpeg` with an APP1 of `len` body bytes (at most 65,533) right after
/// SOI, as an EXIF block with a thumbnail sits before everything else.
pub fn with_app1(jpeg: &[u8], len: usize) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&(len as u16 + 2).to_be_bytes());
    out.extend(
        b"Exif\0\0"
            .iter()
            .copied()
            .chain((0..).map(|i: u32| (i % 251) as u8))
            .take(len),
    );
    out.extend_from_slice(&jpeg[2..]);
    out
}
