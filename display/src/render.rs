use crate::fb::Framebuffer;
use crate::{Rect, HEIGHT, WIDTH};
use embedded_graphics_core::{
    draw_target::DrawTarget,
    pixelcolor::BinaryColor,
    prelude::{OriginDimensions, Size},
    Pixel,
};

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Framebuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x >= 0 && point.y >= 0 {
                self.set_pixel(
                    point.x as usize,
                    point.y as usize,
                    matches!(color, BinaryColor::On),
                );
            }
        }
        Ok(())
    }
}

pub fn fill_rect(fb: &mut Framebuffer, rect: Rect, white: bool) {
    let Some(rect) = rect.clipped() else {
        return;
    };

    let y_end = rect.y as usize + rect.h as usize;
    let x_end = rect.x as usize + rect.w as usize;
    for y in rect.y as usize..y_end {
        for x in rect.x as usize..x_end {
            fb.set_pixel(x, y, white);
        }
    }
}

pub fn stroke_rect(fb: &mut Framebuffer, rect: Rect, white: bool) {
    let Some(rect) = rect.clipped() else {
        return;
    };
    if rect.w == 0 || rect.h == 0 {
        return;
    }

    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let x1 = x0 + rect.w as usize - 1;
    let y1 = y0 + rect.h as usize - 1;

    for x in x0..=x1 {
        fb.set_pixel(x, y0, white);
        fb.set_pixel(x, y1, white);
    }
    for y in y0..=y1 {
        fb.set_pixel(x0, y, white);
        fb.set_pixel(x1, y, white);
    }
}

pub fn draw_ascii(fb: &mut Framebuffer, text: &str, x: usize, y: usize, white: bool) {
    let mut cursor = x;
    for byte in text.bytes() {
        draw_glyph(fb, byte, cursor, y, white);
        cursor += 8;
    }
}

fn draw_glyph(fb: &mut Framebuffer, byte: u8, x: usize, y: usize, white: bool) {
    let glyph = glyph_5x7(byte);
    for (col, bits) in glyph.iter().enumerate() {
        for row in 0..7 {
            if bits & (1 << row) != 0 {
                fb.set_pixel(x + col, y + row, white);
            }
        }
    }
}

pub fn glyph_5x7(byte: u8) -> [u8; 5] {
    match byte {
        b'0' => [0x3E, 0x45, 0x49, 0x51, 0x3E],
        b'1' => [0x00, 0x21, 0x7F, 0x01, 0x00],
        b'2' => [0x21, 0x43, 0x45, 0x49, 0x31],
        b'3' => [0x42, 0x41, 0x51, 0x69, 0x46],
        b'4' => [0x0C, 0x14, 0x24, 0x7F, 0x04],
        b'5' => [0x72, 0x51, 0x51, 0x51, 0x4E],
        b'6' => [0x1E, 0x29, 0x49, 0x49, 0x06],
        b'7' => [0x40, 0x47, 0x48, 0x50, 0x60],
        b'8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        b'9' => [0x30, 0x49, 0x49, 0x4A, 0x3C],
        b'/' => [0x03, 0x04, 0x08, 0x10, 0x60],
        b'A'..=b'Z' => GLYPHS[(byte - b'A') as usize],
        b'a'..=b'z' => GLYPHS[(byte - b'a') as usize],
        b'-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        b'.' => [0x00, 0x03, 0x03, 0x00, 0x00],
        b':' => [0x00, 0x36, 0x36, 0x00, 0x00],
        b'>' => [0x41, 0x22, 0x14, 0x08, 0x00],
        b' ' => [0; 5],
        _ => [0x7F, 0x41, 0x5D, 0x41, 0x7F],
    }
}

const GLYPHS: [[u8; 5]; 26] = [
    [0x3F, 0x44, 0x44, 0x44, 0x3F],
    [0x7F, 0x49, 0x49, 0x49, 0x36],
    [0x3E, 0x41, 0x41, 0x41, 0x22],
    [0x7F, 0x41, 0x41, 0x22, 0x1C],
    [0x7F, 0x49, 0x49, 0x49, 0x41],
    [0x7F, 0x48, 0x48, 0x48, 0x40],
    [0x3E, 0x41, 0x49, 0x49, 0x2E],
    [0x7F, 0x08, 0x08, 0x08, 0x7F],
    [0x00, 0x41, 0x7F, 0x41, 0x00],
    [0x02, 0x01, 0x41, 0x7E, 0x40],
    [0x7F, 0x08, 0x14, 0x22, 0x41],
    [0x7F, 0x01, 0x01, 0x01, 0x01],
    [0x7F, 0x20, 0x18, 0x20, 0x7F],
    [0x7F, 0x10, 0x08, 0x04, 0x7F],
    [0x3E, 0x41, 0x41, 0x41, 0x3E],
    [0x7F, 0x48, 0x48, 0x48, 0x30],
    [0x3E, 0x41, 0x45, 0x42, 0x3D],
    [0x7F, 0x48, 0x4C, 0x4A, 0x31],
    [0x31, 0x49, 0x49, 0x49, 0x46],
    [0x40, 0x40, 0x7F, 0x40, 0x40],
    [0x7E, 0x01, 0x01, 0x01, 0x7E],
    [0x7C, 0x02, 0x01, 0x02, 0x7C],
    [0x7E, 0x01, 0x0E, 0x01, 0x7E],
    [0x63, 0x14, 0x08, 0x14, 0x63],
    [0x70, 0x08, 0x07, 0x08, 0x70],
    [0x43, 0x45, 0x49, 0x51, 0x61],
];
