use crate::fb::Framebuffer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontStyle {
    Regular,
    Italic,
    Bold,
    BoldItalic,
}

/// Escape character that introduces an inline style change in cached reader
/// text. The byte after it is a [`style_marker_code`] digit.
pub const STYLE_MARKER: char = '\u{1b}';

/// Encode a style as the single marker digit stored after [`STYLE_MARKER`]
/// in cached text. Firmware rendering and host preview share this format.
pub fn style_marker_code(style: FontStyle) -> char {
    match style {
        FontStyle::Regular => '0',
        FontStyle::Italic => '1',
        FontStyle::Bold => '2',
        FontStyle::BoldItalic => '3',
    }
}

/// Decode a [`style_marker_code`] digit back into a style.
pub fn style_from_marker_code(code: char) -> Option<FontStyle> {
    match code {
        '0' => Some(FontStyle::Regular),
        '1' => Some(FontStyle::Italic),
        '2' => Some(FontStyle::Bold),
        '3' => Some(FontStyle::BoldItalic),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphMetric {
    pub offset: usize,
    pub len: usize,
    pub width: u8,
    pub height: u8,
    pub x_offset: i8,
    pub y_offset: i8,
    pub advance: u8,
}

pub struct BitmapFont {
    pub codepoints: &'static [u16],
    pub line_height: u8,
    pub baseline: u8,
    pub metrics: &'static [GlyphMetric],
    pub bitmap: &'static [u8],
}

impl BitmapFont {
    pub fn glyph(&self, codepoint: u16) -> Option<(&GlyphMetric, &'static [u8])> {
        let index = self.codepoints.binary_search(&codepoint).ok()?;
        let metric = self.metrics.get(index)?;
        Some((
            metric,
            &self.bitmap[metric.offset..metric.offset + metric.len],
        ))
    }
}

pub fn literata(style: FontStyle) -> &'static BitmapFont {
    match style {
        FontStyle::Regular => &crate::literata_generated::LITERATA_REGULAR,
        FontStyle::Italic => &crate::literata_generated::LITERATA_ITALIC,
        FontStyle::Bold => &crate::literata_generated::LITERATA_BOLD,
        FontStyle::BoldItalic => &crate::literata_generated::LITERATA_BOLD_ITALIC,
    }
}

/// The 16px apparatus set: folios, colophons, margin-key small caps.
pub fn literata_small(style: FontStyle) -> &'static BitmapFont {
    match style {
        FontStyle::Regular => &crate::literata_extra_generated::LITERATA_SMALL_REGULAR,
        FontStyle::Italic => &crate::literata_extra_generated::LITERATA_SMALL_ITALIC,
        FontStyle::Bold | FontStyle::BoldItalic => {
            &crate::literata_extra_generated::LITERATA_SMALL_BOLD
        }
    }
}

/// The 46px display set: the book title on home and the sleep plate.
pub fn literata_display() -> &'static BitmapFont {
    &crate::literata_extra_generated::LITERATA_DISPLAY_REGULAR
}

pub fn draw_text(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    baseline_y: i16,
    white: bool,
) -> i16 {
    let mut cursor = x;
    for ch in text.chars() {
        let codepoint = ch as u32;
        if codepoint > u16::MAX as u32 {
            continue;
        }
        cursor += draw_glyph(fb, font, codepoint as u16, cursor, baseline_y, white);
    }
    cursor
}

pub fn measure_text(font: &BitmapFont, text: &str) -> u16 {
    let fallback = font
        .glyph(b'?' as u16)
        .map(|(m, _)| m.advance as u16)
        .unwrap_or(8);
    text.chars()
        .map(|ch| {
            let codepoint = ch as u32;
            if codepoint > u16::MAX as u32 {
                return fallback;
            }
            font.glyph(codepoint as u16)
                .map(|(metric, _)| metric.advance as u16)
                .unwrap_or(fallback)
        })
        .sum()
}

fn draw_glyph(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    codepoint: u16,
    x: i16,
    baseline_y: i16,
    white: bool,
) -> i16 {
    let Some((metric, bitmap)) = font.glyph(codepoint).or_else(|| font.glyph(b'?' as u16)) else {
        return 8;
    };

    let glyph_x = x + metric.x_offset as i16;
    let glyph_y = baseline_y + metric.y_offset as i16;
    let row_bytes = (metric.width as usize).div_ceil(8);
    for y in 0..metric.height as usize {
        for x_byte in 0..row_bytes {
            let byte = bitmap[y * row_bytes + x_byte];
            if byte == 0 {
                continue;
            }
            for bit in 0..8 {
                let px = x_byte * 8 + bit;
                if px >= metric.width as usize {
                    break;
                }
                if byte & (0x80 >> bit) != 0 {
                    let draw_x = glyph_x + px as i16;
                    let draw_y = glyph_y + y as i16;
                    if draw_x >= 0 && draw_y >= 0 {
                        fb.set_pixel(draw_x as usize, draw_y as usize, white);
                    }
                }
            }
        }
    }

    metric.advance as i16
}

