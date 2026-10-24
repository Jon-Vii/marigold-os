use crate::fb::{Framebuffer, GlyphPainter};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Where a glyph sits and how far it advances: its box is `width` x
/// `height` pixels with its top-left corner `x_offset`, `y_offset` from the
/// pen position on the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphMetric {
    pub width: u8,
    pub height: u8,
    pub x_offset: i8,
    pub y_offset: i8,
    /// Horizontal advance in 12.4 fixed-point pixels.
    pub advance_fp: u16,
}

/// A built-in glyph: its metric and the byte offset of its bitmap record in
/// [`BitmapFont::bitmap`]. Identical glyphs share one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlyphEntry {
    pub bitmap: u16,
    pub metric: GlyphMetric,
}

impl GlyphEntry {
    /// The constructor generated tables use, to keep them short.
    pub const fn new(
        bitmap: u16,
        width: u8,
        height: u8,
        x_offset: i8,
        y_offset: i8,
        advance_fp: u16,
    ) -> Self {
        Self {
            bitmap,
            metric: GlyphMetric {
                width,
                height,
                x_offset,
                y_offset,
                advance_fp,
            },
        }
    }
}

/// A run of consecutive codepoints a font covers, `first..=last`, whose
/// glyph slots start at `slot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodepointRange {
    pub first: u16,
    pub last: u16,
    pub slot: u16,
}

/// The codepoints kerning pairs can name, in kerning-index order: printable
/// ASCII, then the dashes, curly quotes and ellipsis.
/// `tools/fontgen_common.py` writes kerning in this order.
pub const KERNING_CODEPOINTS: [u16; 102] = {
    let mut out = [0u16; 102];
    let mut i = 0;
    while i < 95 {
        out[i] = 0x20 + i as u16;
        i += 1;
    }
    let punctuation = [0x2013, 0x2014, 0x2018, 0x2019, 0x201C, 0x201D, 0x2026];
    let mut j = 0;
    while j < punctuation.len() {
        out[95 + j] = punctuation[j];
        j += 1;
    }
    out
};

/// Entries in [`BitmapFont::kern_rows`]: one start per kerning codepoint
/// plus the end of the last run.
pub const KERN_ROWS: usize = KERNING_CODEPOINTS.len() + 1;

#[inline]
fn kerning_index(codepoint: u16) -> Option<usize> {
    Some(match codepoint {
        0x20..=0x7E => (codepoint - 0x20) as usize,
        0x2013 => 95,
        0x2014 => 96,
        0x2018 => 97,
        0x2019 => 98,
        0x201C => 99,
        0x201D => 100,
        0x2026 => 101,
        _ => return None,
    })
}

/// A typeface at one size, rasterized to 1 bpp by the `tools/generate_*.py`
/// scripts.
///
/// - `ranges` maps a codepoint to its glyph slot.
/// - `present` has a bit per slot (least significant first in each word),
///   set where the face has its own glyph. A clear bit means the face draws
///   its missing-glyph box there, which is the last of `glyphs`, so those
///   slots cost no entry. Empty when every slot is present.
///   `present_rank[w]` counts the set bits before word `w`.
/// - `glyphs` holds the present slots in order, then the missing-glyph box.
/// - `bitmap` holds each glyph's rows, MSB first: per row a `0` bit repeats
///   the row above (an empty row above the first), or a `1` bit is followed
///   by `width` bits of the row. Records start on a byte.
/// - `kern` holds the kerning pairs grouped by left codepoint, in
///   [`KERNING_CODEPOINTS`] order, `kern_rows[i]..kern_rows[i + 1]` for left
///   index `i`. Each pair is `right index << 9` plus the adjustment in 12.4
///   fixed-point pixels as a 9-bit two's complement, sorted by right index.
pub struct BitmapFont {
    pub line_height: u8,
    pub baseline: u8,
    pub ranges: &'static [CodepointRange],
    pub present: &'static [u32],
    pub present_rank: &'static [u16],
    pub glyphs: &'static [GlyphEntry],
    pub bitmap: &'static [u8],
    pub kern_rows: &'static [u16; KERN_ROWS],
    pub kern: &'static [u16],
}

impl BitmapFont {
    #[inline]
    fn slot(&self, codepoint: u16) -> Option<usize> {
        for range in self.ranges {
            if codepoint < range.first {
                return None;
            }
            if codepoint <= range.last {
                return Some((range.slot + (codepoint - range.first)) as usize);
            }
        }
        None
    }

    #[inline]
    fn entry_at(&self, slot: usize) -> Option<&'static GlyphEntry> {
        let glyphs: &'static [GlyphEntry] = self.glyphs;
        if self.present.is_empty() {
            return glyphs.get(slot);
        }
        let word = *self.present.get(slot / 32)?;
        let bit = 1u32 << (slot % 32);
        let index = if word & bit == 0 {
            glyphs.len().checked_sub(1)?
        } else {
            self.present_rank[slot / 32] as usize + (word & (bit - 1)).count_ones() as usize
        };
        glyphs.get(index)
    }

    /// The glyph the face draws for `codepoint`, if it covers it.
    #[inline]
    pub fn entry(&self, codepoint: u16) -> Option<&'static GlyphEntry> {
        self.entry_at(self.slot(codepoint)?)
    }

    /// The metric of the glyph the face draws for `codepoint`, if it covers
    /// it.
    #[inline]
    pub fn metric(&self, codepoint: u16) -> Option<GlyphMetric> {
        self.entry(codepoint).map(|entry| entry.metric)
    }

    /// Every covered codepoint and its metric, in codepoint order.
    pub fn glyphs(&self) -> impl Iterator<Item = (u16, GlyphMetric)> + '_ {
        self.ranges.iter().flat_map(move |range| {
            (range.first..=range.last).filter_map(move |codepoint| {
                let slot = (range.slot + (codepoint - range.first)) as usize;
                Some((codepoint, self.entry_at(slot)?.metric))
            })
        })
    }

    pub fn kerning_adjust_fp(&self, left: u16, right: u16) -> i16 {
        let (Some(left), Some(right)) = (kerning_index(left), kerning_index(right)) else {
            return 0;
        };
        let start = self.kern_rows[left] as usize;
        let end = self.kern_rows[left + 1] as usize;
        let Some(pairs) = self.kern.get(start..end) else {
            return 0;
        };
        let right = right as u16;
        match pairs.binary_search_by(|pair| (pair >> 9).cmp(&right)) {
            // Sign-extend the low 9 bits.
            Ok(index) => ((pairs[index] << 7) as i16) >> 7,
            Err(_) => 0,
        }
    }

    /// Decode a glyph's rows top to bottom: `row(y, bits)` with the row's
    /// pixels from bit 63 down. Glyphs are at most 64 pixels wide.
    #[inline]
    pub fn rows(&self, entry: &GlyphEntry, mut row: impl FnMut(u32, u64)) {
        let (width, height) = (entry.metric.width as u32, entry.metric.height as u32);
        if width == 0 || height == 0 || width > 64 {
            return;
        }
        let mut bits = Bits::new(self.bitmap.get(entry.bitmap as usize..).unwrap_or(&[]));
        let mut previous = 0u64;
        // One call site for `row`, so it inlines into the glyph loop.
        for y in 0..height {
            bits.refill();
            if bits.take(1) == 1 {
                previous = if width <= 24 {
                    ((bits.take(width) << (32 - width)) as u64) << 32
                } else {
                    bits.wide(width)
                };
            }
            row(y, previous);
        }
    }
}

/// MSB-first reader over a glyph's bitmap record. Past the end of the
/// table it reads zeros.
struct Bits<'a> {
    data: &'a [u8],
    next: usize,
    buffer: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    #[inline]
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            next: 0,
            buffer: 0,
            count: 0,
        }
    }

    /// Buffer at least 25 bits.
    #[inline]
    fn refill(&mut self) {
        while self.count <= 24 {
            let byte = self.data.get(self.next).copied().unwrap_or(0);
            self.next += 1;
            self.buffer |= (byte as u32) << (24 - self.count);
            self.count += 8;
        }
    }

    /// The next `n` (1..=25) bits; `refill` first.
    #[inline]
    fn take(&mut self, n: u32) -> u32 {
        let value = self.buffer >> (32 - n);
        self.buffer <<= n;
        self.count -= n;
        value
    }

    /// A row 25..=64 pixels wide, MSB-first in a u64. Out of line: wide
    /// glyphs are rare.
    #[inline(never)]
    fn wide(&mut self, width: u32) -> u64 {
        let mut value = 0u64;
        let mut left = width;
        while left > 0 {
            self.refill();
            let n = left.min(16);
            value = (value << n) | self.take(n) as u64;
            left -= n;
        }
        value << (64 - width)
    }
}

/// Reader body size behind the Type Size setting. UI furniture keeps the
/// fixed [`literata`] (Medium) set; only reading content scales.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
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

/// How body paragraphs set their lines, behind the Alignment setting.
/// Justified fills each line to the measure and hyphenates to keep the
/// gaps even; Left keeps words whole and spacing natural, leaving a ragged
/// right edge. Only text the book sets justified (the body default) follows
/// it; centred and left-set text keep the book's alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Alignment {
    #[default]
    Justified,
    Left,
}

impl Alignment {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Justified),
            1 => Some(Self::Left),
            _ => None,
        }
    }
}

/// The reader type settings that change page layout. Carried from the app
/// reducer through storage commands into the cache build, so pagination,
/// cached sections, and drawing always agree on one set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TypeSettings {
    pub size: FontSize,
    pub spacing: LineSpacing,
    pub weight: FontWeight,
    pub family: FontFamily,
    pub alignment: Alignment,
}

impl TypeSettings {
    pub const DEFAULT: Self = Self {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
        alignment: Alignment::Justified,
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
    let fallback = font.entry(b'?' as u16);
    for ch in text.chars() {
        // Characters past the BMP have no glyphs; draw the same `?` that
        // `measure_text` counts, so centred and right-aligned text lines up.
        let codepoint = u16::try_from(ch as u32).unwrap_or(b'?' as u16);
        let (drawn, entry) = match font.entry(codepoint) {
            Some(entry) => (codepoint, Some(entry)),
            None => (b'?' as u16, fallback),
        };
        if let Some(left) = previous {
            cursor_fp += font.kerning_adjust_fp(left, drawn) as i32;
        }
        let x = fixed_round(cursor_fp);
        cursor_fp += match entry {
            Some(entry) => {
                draw_glyph(fb, font, entry, x, baseline_y, white);
                entry.metric.advance_fp as i32
            }
            None => 8 << 4,
        };
        previous = Some(drawn);
    }
    fixed_round(cursor_fp)
}

pub fn measure_text(font: &BitmapFont, text: &str) -> u16 {
    let fallback = font.metric(b'?' as u16);
    let mut advance_fp = 0i32;
    let mut previous = None;
    for ch in text.chars() {
        let codepoint = if ch as u32 > u16::MAX as u32 {
            b'?' as u16
        } else {
            ch as u16
        };
        let (advance, measured) = if let Some(metric) = font.metric(codepoint) {
            (metric.advance_fp, codepoint)
        } else if let Some(metric) = fallback {
            (metric.advance_fp, b'?' as u16)
        } else {
            (8 << 4, b'?' as u16)
        };
        if let Some(left) = previous {
            advance_fp += font.kerning_adjust_fp(left, measured) as i32;
        }
        advance_fp += advance as i32;
        previous = Some(measured);
    }
    fixed_ceil(advance_fp).max(0) as u16
}

/// Paint one glyph with its pen position at `x` on the baseline.
#[inline]
fn draw_glyph(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    entry: &GlyphEntry,
    x: i16,
    baseline_y: i16,
    white: bool,
) {
    let metric = entry.metric;
    let mut painter = GlyphPainter::new(
        fb,
        x as i32 + metric.x_offset as i32,
        baseline_y as i32 + metric.y_offset as i32,
        metric.width as u32,
        metric.height as u32,
        white,
    );
    font.rows(entry, |row, bits| painter.row(row, bits));
    painter.finish();
}

#[inline]
pub fn fixed_round(value_fp: i32) -> i16 {
    ((value_fp + 8) >> 4) as i16
}

#[inline]
pub fn fixed_ceil(value_fp: i32) -> i16 {
    ((value_fp + 15) >> 4) as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fb::FbFrame;

    #[test]
    fn drawing_advances_exactly_as_far_as_measuring() {
        // Characters past the BMP are measured as `?`; drawing must advance
        // the same distance or centred labels drift.
        let font = literata(FontStyle::Regular);
        let mut fb = Framebuffer::new();
        for text in ["plain", "a\u{1F600}b", "\u{1F4D6} title"] {
            let end = draw_text(&mut fb, font, text, 0, 40, false) as i32;
            // Measuring rounds the final advance up, drawing to nearest.
            let measured = measure_text(font, text) as i32;
            assert!(
                (0..=1).contains(&(measured - end)),
                "{text}: {measured} vs {end}"
            );
        }
    }

    /// Every built-in face, for the table checks below.
    fn all_fonts() -> std::vec::Vec<&'static BitmapFont> {
        use crate::literata_extra_generated as extra;
        let mut fonts = std::vec::Vec::new();
        for size in [FontSize::Small, FontSize::Medium, FontSize::Large] {
            for style in [
                FontStyle::Regular,
                FontStyle::Italic,
                FontStyle::Bold,
                FontStyle::BoldItalic,
            ] {
                fonts.push(literata_sized(size, style));
                fonts.push(merriweather_sized(size, style));
            }
            fonts.push(semibold_sized(size, false));
            fonts.push(semibold_sized(size, true));
        }
        fonts.extend([
            &extra::LITERATA_SMALL_REGULAR,
            &extra::LITERATA_SMALL_ITALIC,
            &extra::LITERATA_SMALL_BOLD,
            &extra::LITERATA_DISPLAY_REGULAR,
        ]);
        fonts
    }

    #[test]
    fn tables_are_consistent() {
        let fonts = all_fonts();
        assert_eq!(fonts.len(), 34);
        for font in fonts {
            let slots: usize = font
                .ranges
                .iter()
                .map(|r| (r.last - r.first) as usize + 1)
                .sum();
            assert_eq!(font.glyphs().count(), slots);
            if !font.present.is_empty() {
                assert_eq!(font.present.len(), slots.div_ceil(32));
                let present: u32 = font.present.iter().map(|w| w.count_ones()).sum();
                assert_eq!(present as usize + 1, font.glyphs.len());
            }
            for entry in font.glyphs {
                let mut rows = 0;
                font.rows(entry, |y, bits| {
                    assert_eq!(y, rows);
                    rows += 1;
                    let width = entry.metric.width as u32;
                    assert_eq!(bits.checked_shl(width).unwrap_or(0), 0);
                });
                let drawn = entry.metric.width > 0 && entry.metric.height > 0;
                assert_eq!(rows, if drawn { entry.metric.height as u32 } else { 0 });
            }
            assert_eq!(font.kern_rows[0], 0);
            assert_eq!(font.kern_rows[KERN_ROWS - 1] as usize, font.kern.len());
            for run in font.kern_rows.windows(2) {
                let pairs = &font.kern[run[0] as usize..run[1] as usize];
                assert!(pairs.windows(2).all(|p| p[0] >> 9 < p[1] >> 9));
            }
        }
    }

    /// The glyph painted `set_pixel` by `set_pixel`, clipped the way the
    /// glyph loop always clipped: nothing left of or above the frame, and
    /// `set_pixel` drops anything past its far edges.
    fn reference_glyph(
        fb: &mut Framebuffer,
        font: &BitmapFont,
        entry: &GlyphEntry,
        x: i32,
        y: i32,
        white: bool,
    ) {
        font.rows(entry, |row, bits| {
            for column in 0..entry.metric.width as i32 {
                if bits & (1u64 << (63 - column)) != 0 {
                    let (px, py) = (x + column, y + row as i32);
                    if px >= 0 && py >= 0 {
                        fb.set_pixel(px as usize, py as usize, white);
                    }
                }
            }
        });
    }

    fn painted_glyph(
        fb: &mut Framebuffer,
        font: &BitmapFont,
        entry: &GlyphEntry,
        x: i32,
        y: i32,
        white: bool,
    ) {
        let metric = entry.metric;
        let mut painter =
            GlyphPainter::new(fb, x, y, metric.width as u32, metric.height as u32, white);
        font.rows(entry, |row, bits| painter.row(row, bits));
        painter.finish();
    }

    /// Places for a batch of glyphs: a grid inside the frame at every start
    /// bit, then every edge and corner, half in and half out, and past each
    /// far edge.
    fn placements(frame_w: i32, frame_h: i32, w: i32, h: i32, index: usize) -> [(i32, i32); 10] {
        let k = index as i32;
        let along_x = (k * 37) % (frame_w - 64).max(1);
        let along_y = (k * 23) % (frame_h - 64).max(1);
        [
            (
                8 + (k * 61) % (frame_w - 80) + k % 8,
                8 + (k * 29) % (frame_h - 80),
            ),
            (-w / 2, along_y),
            (frame_w - w / 2, along_y),
            (along_x, -h / 2),
            (along_x, frame_h - h / 2),
            (-w / 2, -h / 2),
            (frame_w - w / 2, frame_h - h / 2),
            (frame_w - w + 1, along_y),
            (frame_w, along_y),
            (along_x, frame_h),
        ]
    }

    #[test]
    fn painter_matches_set_pixel_for_every_glyph_in_every_frame() {
        let frames = [
            FbFrame::Native,
            FbFrame::Landscape,
            FbFrame::LandscapeFlipped,
            FbFrame::Portrait,
        ];
        for (f, font) in all_fonts().into_iter().enumerate() {
            for frame in frames {
                // White ink on black for some faces, so both stores are held
                // to the reference.
                let white = f % 5 == 4;
                let mut painted = Framebuffer::new();
                let mut reference = Framebuffer::new();
                for fb in [&mut painted, &mut reference] {
                    fb.set_frame(frame);
                    fb.clear(!white);
                }
                let (fw, fh) = (painted.frame_width() as i32, painted.frame_height() as i32);
                let mut drawn = 0;
                for (index, entry) in font.glyphs.iter().enumerate() {
                    let (w, h) = (entry.metric.width as i32, entry.metric.height as i32);
                    for (x, y) in placements(fw, fh, w, h, index) {
                        painted_glyph(&mut painted, font, entry, x, y, white);
                        reference_glyph(&mut reference, font, entry, x, y, white);
                    }
                    drawn += 1;
                    // Compare in small batches, so overlapping ink can't
                    // hide a wrong pixel for long.
                    if drawn % 8 == 0 || index + 1 == font.glyphs.len() {
                        assert!(
                            painted.bytes()[..] == reference.bytes()[..],
                            "font {f} {frame:?}: glyphs up to {index} differ"
                        );
                        for fb in [&mut painted, &mut reference] {
                            fb.clear(!white);
                        }
                    }
                }
            }
        }
    }
}
