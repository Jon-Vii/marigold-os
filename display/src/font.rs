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

/// Per-glyph bitmap location and layout. Field widths match the SD
/// font-pack metric record (`proto::font_pack::FONT_PACK_METRIC_BYTES`):
/// 12 bytes instead of the padded 16 that `usize` offsets cost, which
/// saves ~195 KB of flash across the ~50k built-in glyph entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphMetric {
    pub offset: u32,
    pub len: u16,
    pub width: u8,
    pub height: u8,
    pub x_offset: i8,
    pub y_offset: i8,
    /// Horizontal advance in 12.4 fixed-point pixels.
    pub advance_fp: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KerningEntry {
    pub left: u16,
    pub right: u16,
    /// Kerning adjustment in 12.4 fixed-point pixels.
    pub adjust_fp: i16,
}

pub struct BitmapFont {
    pub codepoints: &'static [u16],
    pub line_height: u8,
    pub baseline: u8,
    pub metrics: &'static [GlyphMetric],
    pub bitmap: &'static [u8],
    pub kerning: &'static [KerningEntry],
}

impl BitmapFont {
    pub fn glyph(&self, codepoint: u16) -> Option<(&GlyphMetric, &'static [u8])> {
        let index = self.codepoints.binary_search(&codepoint).ok()?;
        let metric = self.metrics.get(index)?;
        let offset = metric.offset as usize;
        Some((metric, &self.bitmap[offset..offset + metric.len as usize]))
    }

    pub fn kerning_adjust_fp(&self, left: u16, right: u16) -> i16 {
        self.kerning
            .binary_search_by(|entry| (entry.left, entry.right).cmp(&(left, right)))
            .map(|index| self.kerning[index].adjust_fp)
            .unwrap_or(0)
    }
}

/// Reader body size behind the Type Size setting. UI furniture keeps the
/// fixed [`literata`] (Medium) set; only reading content scales.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl FontSize {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Small),
            1 => Some(Self::Medium),
            2 => Some(Self::Large),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineSpacing {
    Compact,
    #[default]
    Normal,
    Relaxed,
}

impl LineSpacing {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Compact),
            1 => Some(Self::Normal),
            2 => Some(Self::Relaxed),
            _ => None,
        }
    }
}

/// Reader body weight behind the Type Weight setting. `Heavy` renders regular
/// prose one step up (Literata SemiBold) for easier reading; bold emphasis
/// keeps the heavier Bold face so it stays distinct. UI furniture is
/// unaffected — only reading content changes weight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontWeight {
    #[default]
    Normal,
    Heavy,
}

impl FontWeight {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::Heavy),
            _ => None,
        }
    }
}

/// Reader body typeface behind the Font setting. Literata is the shipped
/// default; Merriweather is the open second face. UI furniture stays
/// Literata — only reading content changes family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontFamily {
    #[default]
    Literata,
    Merriweather,
    Custom,
}

impl FontFamily {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Literata),
            1 => Some(Self::Merriweather),
            2 => Some(Self::Custom),
            _ => None,
        }
    }
}

/// The reader type settings that change page layout. Carried from the app
/// reducer through storage commands into the cache build, so pagination,
/// cached sections, and drawing always agree on one set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeSettings {
    pub size: FontSize,
    pub spacing: LineSpacing,
    pub weight: FontWeight,
    pub family: FontFamily,
}

impl TypeSettings {
    pub const DEFAULT: Self = Self {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
    };
}

pub fn literata(style: FontStyle) -> &'static BitmapFont {
    match style {
        FontStyle::Regular => &crate::literata_generated::LITERATA_REGULAR,
        FontStyle::Italic => &crate::literata_generated::LITERATA_ITALIC,
        FontStyle::Bold => &crate::literata_generated::LITERATA_BOLD,
        FontStyle::BoldItalic => &crate::literata_generated::LITERATA_BOLD_ITALIC,
    }
}

/// The reading body face at a user-selected size: 19px, 22px, or 26px.
pub fn literata_sized(size: FontSize, style: FontStyle) -> &'static BitmapFont {
    use crate::literata_sizes_generated as sizes;
    match (size, style) {
        (FontSize::Medium, _) => literata(style),
        (FontSize::Small, FontStyle::Regular) => &sizes::LITERATA_19_REGULAR,
        (FontSize::Small, FontStyle::Italic) => &sizes::LITERATA_19_ITALIC,
        (FontSize::Small, FontStyle::Bold) => &sizes::LITERATA_19_BOLD,
        (FontSize::Small, FontStyle::BoldItalic) => &sizes::LITERATA_19_BOLD_ITALIC,
        (FontSize::Large, FontStyle::Regular) => &sizes::LITERATA_26_REGULAR,
        (FontSize::Large, FontStyle::Italic) => &sizes::LITERATA_26_ITALIC,
        (FontSize::Large, FontStyle::Bold) => &sizes::LITERATA_26_BOLD,
        (FontSize::Large, FontStyle::BoldItalic) => &sizes::LITERATA_26_BOLD_ITALIC,
    }
}

/// The reading body face for a size, weight, and style run. `Heavy` renders
/// regular and italic prose in SemiBold; bold emphasis keeps the Bold face so
/// it stays a step heavier than the surrounding heavier body.
pub fn literata_weighted(
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> &'static BitmapFont {
    match weight {
        FontWeight::Normal => literata_sized(size, style),
        FontWeight::Heavy => match style {
            FontStyle::Regular => semibold_sized(size, false),
            FontStyle::Italic => semibold_sized(size, true),
            FontStyle::Bold => literata_sized(size, FontStyle::Bold),
            FontStyle::BoldItalic => literata_sized(size, FontStyle::BoldItalic),
        },
    }
}

/// The Merriweather reading face at a user-selected size: 19px, 22px, or 26px.
pub fn merriweather_sized(size: FontSize, style: FontStyle) -> &'static BitmapFont {
    use crate::merriweather_generated as mw;
    match (size, style) {
        (FontSize::Small, FontStyle::Regular) => &mw::MERRIWEATHER_19_REGULAR,
        (FontSize::Small, FontStyle::Italic) => &mw::MERRIWEATHER_19_ITALIC,
        (FontSize::Small, FontStyle::Bold) => &mw::MERRIWEATHER_19_BOLD,
        (FontSize::Small, FontStyle::BoldItalic) => &mw::MERRIWEATHER_19_BOLD_ITALIC,
        (FontSize::Medium, FontStyle::Regular) => &mw::MERRIWEATHER_22_REGULAR,
        (FontSize::Medium, FontStyle::Italic) => &mw::MERRIWEATHER_22_ITALIC,
        (FontSize::Medium, FontStyle::Bold) => &mw::MERRIWEATHER_22_BOLD,
        (FontSize::Medium, FontStyle::BoldItalic) => &mw::MERRIWEATHER_22_BOLD_ITALIC,
        (FontSize::Large, FontStyle::Regular) => &mw::MERRIWEATHER_26_REGULAR,
        (FontSize::Large, FontStyle::Italic) => &mw::MERRIWEATHER_26_ITALIC,
        (FontSize::Large, FontStyle::Bold) => &mw::MERRIWEATHER_26_BOLD,
        (FontSize::Large, FontStyle::BoldItalic) => &mw::MERRIWEATHER_26_BOLD_ITALIC,
    }
}

/// The reading body face for the full type settings and a style run. The
/// Merriweather set carries no SemiBold, so its `Heavy` promotes regular and
/// italic prose to the Bold face; bold emphasis then blends into the heavier
/// body, the same tradeoff the Literata path makes only at Bold rather than a
/// dedicated SemiBold.
pub fn family_weighted(
    family: FontFamily,
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> &'static BitmapFont {
    match family {
        FontFamily::Literata => literata_weighted(size, weight, style),
        FontFamily::Merriweather => match weight {
            FontWeight::Normal => merriweather_sized(size, style),
            FontWeight::Heavy => match style {
                FontStyle::Regular => merriweather_sized(size, FontStyle::Bold),
                FontStyle::Italic => merriweather_sized(size, FontStyle::BoldItalic),
                _ => merriweather_sized(size, style),
            },
        },
        FontFamily::Custom => custom_weighted(size, weight, style),
    }
}

pub fn builtin_custom_available() -> bool {
    cfg!(feature = "builtin-custom-font")
}

#[cfg(feature = "builtin-custom-font")]
pub fn builtin_custom_name() -> &'static str {
    crate::custom_generated::CUSTOM_FONT_NAME
}

#[cfg(not(feature = "builtin-custom-font"))]
pub fn builtin_custom_name() -> &'static str {
    ""
}

#[cfg(feature = "builtin-custom-font")]
pub fn builtin_custom_identity() -> u64 {
    crate::custom_generated::CUSTOM_FONT_IDENTITY
}

#[cfg(not(feature = "builtin-custom-font"))]
pub fn builtin_custom_identity() -> u64 {
    0
}

#[cfg(feature = "builtin-custom-font")]
pub fn custom_weighted(
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> &'static BitmapFont {
    use crate::custom_generated as custom;
    let style = match (weight, style) {
        (FontWeight::Normal, style) => style,
        (FontWeight::Heavy, FontStyle::Regular | FontStyle::Bold) => FontStyle::Bold,
        (FontWeight::Heavy, FontStyle::Italic | FontStyle::BoldItalic) => FontStyle::BoldItalic,
    };
    match (size, style) {
        (FontSize::Small, FontStyle::Regular) => &custom::CUSTOM_19_REGULAR,
        (FontSize::Small, FontStyle::Italic) => &custom::CUSTOM_19_ITALIC,
        (FontSize::Small, FontStyle::Bold) => &custom::CUSTOM_19_BOLD,
        (FontSize::Small, FontStyle::BoldItalic) => &custom::CUSTOM_19_BOLD_ITALIC,
        (FontSize::Medium, FontStyle::Regular) => &custom::CUSTOM_22_REGULAR,
        (FontSize::Medium, FontStyle::Italic) => &custom::CUSTOM_22_ITALIC,
        (FontSize::Medium, FontStyle::Bold) => &custom::CUSTOM_22_BOLD,
        (FontSize::Medium, FontStyle::BoldItalic) => &custom::CUSTOM_22_BOLD_ITALIC,
        (FontSize::Large, FontStyle::Regular) => &custom::CUSTOM_26_REGULAR,
        (FontSize::Large, FontStyle::Italic) => &custom::CUSTOM_26_ITALIC,
        (FontSize::Large, FontStyle::Bold) => &custom::CUSTOM_26_BOLD,
        (FontSize::Large, FontStyle::BoldItalic) => &custom::CUSTOM_26_BOLD_ITALIC,
    }
}

#[cfg(not(feature = "builtin-custom-font"))]
pub fn custom_weighted(
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> &'static BitmapFont {
    // Runtime SD-backed custom fonts are handled by firmware-specific
    // providers. Host-only and non-reader paths keep a total selector.
    literata_weighted(size, weight, style)
}

/// The SemiBold body face at a reading size, upright or italic.
fn semibold_sized(size: FontSize, italic: bool) -> &'static BitmapFont {
    use crate::literata_semibold_generated as sb;
    match (size, italic) {
        (FontSize::Small, false) => &sb::LITERATA_19_SEMIBOLD,
        (FontSize::Small, true) => &sb::LITERATA_19_SEMIBOLD_ITALIC,
        (FontSize::Medium, false) => &sb::LITERATA_22_SEMIBOLD,
        (FontSize::Medium, true) => &sb::LITERATA_22_SEMIBOLD_ITALIC,
        (FontSize::Large, false) => &sb::LITERATA_26_SEMIBOLD,
        (FontSize::Large, true) => &sb::LITERATA_26_SEMIBOLD_ITALIC,
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
    let mut cursor_fp = (x as i32) << 4;
    let mut previous = None;
    for ch in text.chars() {
        let codepoint = ch as u32;
        if codepoint > u16::MAX as u32 {
            continue;
        }
        let codepoint = codepoint as u16;
        let drawn = if font.glyph(codepoint).is_some() {
            codepoint
        } else {
            b'?' as u16
        };
        if let Some(left) = previous {
            cursor_fp += font.kerning_adjust_fp(left, drawn) as i32;
        }
        let x = fixed_round(cursor_fp);
        let (advance_fp, drawn) = draw_glyph(fb, font, codepoint, x, baseline_y, white);
        cursor_fp += advance_fp as i32;
        previous = Some(drawn);
    }
    fixed_round(cursor_fp)
}

pub fn measure_text(font: &BitmapFont, text: &str) -> u16 {
    let fallback = font
        .glyph(b'?' as u16)
        .map(|(m, _)| m.advance_fp)
        .unwrap_or(8 << 4);
    let mut advance_fp = 0i32;
    let mut previous = None;
    for ch in text.chars() {
        let codepoint = if ch as u32 > u16::MAX as u32 {
            b'?' as u16
        } else {
            ch as u16
        };
        let (advance, measured) = if let Some((metric, _)) = font.glyph(codepoint) {
            (metric.advance_fp, codepoint)
        } else if let Some((metric, _)) = font.glyph(b'?' as u16) {
            (metric.advance_fp, b'?' as u16)
        } else {
            (fallback, b'?' as u16)
        };
        if let Some(left) = previous {
            advance_fp += font.kerning_adjust_fp(left, measured) as i32;
        }
        advance_fp += advance as i32;
        previous = Some(measured);
    }
    fixed_ceil(advance_fp).max(0) as u16
}

fn draw_glyph(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    codepoint: u16,
    x: i16,
    baseline_y: i16,
    white: bool,
) -> (u16, u16) {
    let (drawn, metric, bitmap) = if let Some((metric, bitmap)) = font.glyph(codepoint) {
        (codepoint, metric, bitmap)
    } else if let Some((metric, bitmap)) = font.glyph(b'?' as u16) {
        (b'?' as u16, metric, bitmap)
    } else {
        return (8 << 4, b'?' as u16);
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

    (metric.advance_fp, drawn)
}

#[inline]
pub fn fixed_round(value_fp: i32) -> i16 {
    ((value_fp + 8) >> 4) as i16
}

#[inline]
pub fn fixed_ceil(value_fp: i32) -> i16 {
    ((value_fp + 15) >> 4) as i16
}
