//! Shared reading-surface layout: page bounds, type metrics, styled-text
//! measurement, and line wrapping. This is the "reader page plan" seam:
//! firmware reading views, cache building, and host preview tooling must
//! all agree on these numbers and this wrap behavior, so they live here.
//!
//! Measurement is incremental: width accumulates per character instead of
//! re-measuring a whole candidate line per word, which keeps wrapping O(n)
//! in text length.

use display::fb::Framebuffer;
use display::font::{
    draw_text, literata_sized, measure_text, style_from_marker_code, BitmapFont, FontSize,
    FontStyle, LineSpacing, TypeSettings, STYLE_MARKER,
};
use proto::cache::{BlockRecord, PageRecord};
use proto::text::{TextAlign, TextRole};

/// Narrow read model for the reader page plan: bounded block records,
/// their cached text, and pagination flags. Firmware's ReaderStore and
/// host-side fixtures both implement it, so pagination and page drawing
/// cannot drift between device and tools.
pub trait ReadingBlocks {
    fn block_count(&self) -> usize;
    /// Record at `index` while it is inside the live block range.
    fn block(&self, index: usize) -> Option<BlockRecord>;
    fn block_text(&self, index: usize) -> &str;
    fn block_style(&self, index: usize) -> FontStyle;
    fn page_break_before(&self, index: usize) -> bool;
    fn paragraph_end(&self, index: usize) -> bool;
    /// Type settings the blocks were laid out under. Every height,
    /// pagination, and drawing call in this module reads them from the
    /// source, so a store can never paginate with one size and draw with
    /// another.
    fn type_settings(&self) -> TypeSettings {
        TypeSettings::DEFAULT
    }
}

pub struct ReaderDrawableBlock<'a> {
    pub record: BlockRecord,
    pub text: &'a str,
    pub y: i16,
    pub advance: i16,
    pub style: FontStyle,
    pub font: &'static BitmapFont,
}

pub fn block_height(source: &impl ReadingBlocks, index: usize) -> i16 {
    let Some(record) = source.block(index) else {
        return 0;
    };
    let settings = source.type_settings();
    let advance = line_advance(settings, record.role);
    let height = if record.line_count == 1 {
        advance
    } else {
        wrapped_block_height(
            body_font(settings, source.block_style(index)),
            source.block_text(index),
            record.role,
            record.align,
            advance,
        )
    };
    height + paragraph_gap_after(source, index)
}

pub fn paragraph_gap_after(source: &impl ReadingBlocks, index: usize) -> i16 {
    if source.paragraph_end(index) {
        paragraph_gap(
            source
                .block(index)
                .map(|record| record.role)
                .unwrap_or(TextRole::Body),
        )
    } else {
        0
    }
}

/// Count the pages the loaded blocks paginate into, using the same height
/// math as rendering.
pub fn paginate_block_pages(source: &impl ReadingBlocks, page_top: i16, page_bottom: i16) -> usize {
    let mut pages = 1u32;
    let mut y = page_top;

    for index in 0..source.block_count() {
        if source.page_break_before(index) && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        let height = block_height(source, index);

        if y + height > page_bottom && y > page_top {
            pages = pages.saturating_add(1);
            y = page_top;
        }
        y += height;
    }

    pages.max(1) as usize
}

/// Walk the blocks until `page_index` and return its page record.
pub fn page_record_at(
    source: &impl ReadingBlocks,
    page_index: usize,
    page_top: i16,
    page_bottom: i16,
) -> PageRecord {
    let mut current = 0usize;
    let mut first_block = 0usize;
    let mut block_count = 0usize;
    let mut y = page_top;

    for index in 0..source.block_count() {
        let height = block_height(source, index);
        let new_page =
            (y + height > page_bottom || source.page_break_before(index)) && y > page_top;
        if new_page {
            if current == page_index {
                return PageRecord {
                    first_block: first_block as u16,
                    block_count: block_count as u16,
                };
            }
            current += 1;
            first_block = index;
            block_count = 0;
            y = page_top;
        }
        block_count += 1;
        y += height;
    }

    PageRecord {
        first_block: first_block as u16,
        block_count: block_count as u16,
    }
}

pub fn for_each_drawable_block(
    source: &impl ReadingBlocks,
    page: PageRecord,
    mut visit: impl FnMut(ReaderDrawableBlock<'_>) -> bool,
) {
    let settings = source.type_settings();
    let mut y = READER_PAGE_TOP;
    for offset in 0..page.block_count as usize {
        let index = page.first_block as usize + offset;
        let Some(record) = source.block(index) else {
            break;
        };
        let text = source.block_text(index);
        let advance = line_advance(settings, record.role);
        let style = source.block_style(index);
        let height = block_height(source, index);
        if y + height > READER_PAGE_BOTTOM && y > READER_PAGE_TOP {
            break;
        }
        if !visit(ReaderDrawableBlock {
            record,
            text,
            y: y + advance,
            advance,
            style,
            font: body_font(settings, style),
        }) {
            break;
        }
        y += height;
    }
}

/// Draw one page of reading-body blocks: the single rendering of cached
/// reader content shared by firmware views and host tooling.
pub fn draw_reading_page_body(
    fb: &mut Framebuffer,
    source: &impl ReadingBlocks,
    page: PageRecord,
) {
    let settings = source.type_settings();
    for_each_drawable_block(source, page, |block| {
        let role = block.record.role;
        match block.record.align {
            TextAlign::Left => {
                let x = reader_x_for(role);
                if block.record.line_count == 1 {
                    draw_styled_line(fb, settings, block.text, x, block.y, block.style);
                } else {
                    draw_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        READER_RIGHT_X,
                        block.advance,
                    );
                }
            }
            TextAlign::Justify => {
                let x = reader_x_for(role);
                if block.record.line_count == 1 {
                    draw_styled_line(fb, settings, block.text, x, block.y, block.style);
                } else {
                    draw_justified_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        READER_RIGHT_X,
                        block.advance,
                    );
                }
            }
            TextAlign::Center => {
                if block.record.line_count == 1 {
                    let width = styled_text_ink_width(block.text, settings, block.style)
                        .min(READER_RIGHT_X - READER_LEFT_X);
                    let x = ((display::WIDTH as i16 - width) / 2).max(READER_LEFT_X);
                    draw_styled_line(fb, settings, block.text, x, block.y, block.style);
                } else {
                    draw_centered_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        block.y,
                        READER_RIGHT_X - READER_LEFT_X,
                        block.advance,
                    );
                }
            }
        };
        true
    });
}

// Sized so the default 27px body grid closes: 3 + 17*27 = 462 ≤ 463 puts
// seventeen lines on the page where 6/-4 fit sixteen and left ~28px (one
// full row) dead above the footer. Last-line descenders reach ~467,
// clearing the digits-only counter whose ink starts at ~468.
pub const READER_PAGE_TOP: i16 = 3;
pub const READER_FOOTER_TOP: i16 = 466;
pub const READER_PAGE_BOTTOM: i16 = READER_FOOTER_TOP - 3;
pub const READER_LEFT_X: i16 = 8;
pub const READER_RIGHT_X: i16 = 792;
pub const READER_WRAP_SAFETY: i16 = 4;
/// Version of the wrap rules and page constants in this module. Bump when
/// layout changes for unchanged type settings.
const READER_LAYOUT_VERSION: u16 = 6;

/// Section cache layout config: the wrap-rule version plus the type
/// settings the section was paginated under. Stored in cache headers; a
/// mismatch on load invalidates the cached pagination and rebuilds it.
pub fn reader_layout_config(settings: TypeSettings) -> u16 {
    (READER_LAYOUT_VERSION << 4) | ((settings.size as u16) << 2) | settings.spacing as u16
}

/// The reading body face for the given settings and style run.
pub fn body_font(settings: TypeSettings, style: FontStyle) -> &'static BitmapFont {
    literata_sized(settings.size, style)
}

/// Baseline-to-baseline advance. Body values per (size, spacing); H1/H2
/// carry extra lead. Medium/Normal preserves the historical 27/32.
pub fn line_advance(settings: TypeSettings, role: TextRole) -> i16 {
    let body = match (settings.size, settings.spacing) {
        (FontSize::Small, LineSpacing::Compact) => 22,
        (FontSize::Small, LineSpacing::Normal) => 24,
        (FontSize::Small, LineSpacing::Relaxed) => 28,
        (FontSize::Medium, LineSpacing::Compact) => 25,
        (FontSize::Medium, LineSpacing::Normal) => 27,
        (FontSize::Medium, LineSpacing::Relaxed) => 31,
        (FontSize::Large, LineSpacing::Compact) => 29,
        (FontSize::Large, LineSpacing::Normal) => 32,
        (FontSize::Large, LineSpacing::Relaxed) => 36,
    };
    if matches!(role, TextRole::Heading1 | TextRole::Heading2) {
        body + 5
    } else {
        body
    }
}

pub fn paragraph_gap(role: TextRole) -> i16 {
    match role {
        TextRole::Heading1 | TextRole::Heading2 => 10,
        TextRole::Heading3 => 6,
        TextRole::BlockQuote => 6,
        TextRole::Body => 3,
    }
}

pub fn reader_x_for(role: TextRole) -> i16 {
    if matches!(role, TextRole::BlockQuote) {
        32
    } else {
        READER_LEFT_X
    }
}

/// Running ink measurement: pen advance plus the rightmost inked edge,
/// which can exceed the advance for glyphs whose bitmap overhangs their
/// advance width (italics, some punctuation).
#[derive(Clone, Copy, Debug, Default)]
pub struct InkCursor {
    advance: i16,
    right: i16,
}

impl InkCursor {
    pub const fn new() -> Self {
        Self {
            advance: 0,
            right: 0,
        }
    }

    #[inline]
    pub fn push_char(&mut self, font: &BitmapFont, ch: char) {
        let codepoint = if ch as u32 > u16::MAX as u32 {
            b'?' as u16
        } else {
            ch as u16
        };
        let Some((metric, _)) = font.glyph(codepoint).or_else(|| font.glyph(b'?' as u16)) else {
            self.advance += 8;
            self.right = self.right.max(self.advance);
            return;
        };
        let glyph_right = self.advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance += metric.advance as i16;
    }

    pub fn width(&self) -> i16 {
        self.right.max(self.advance)
    }
}

pub fn text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
    let mut ink = InkCursor::new();
    for ch in text.chars() {
        ink.push_char(font, ch);
    }
    ink.width()
}

/// Incremental ink measurement over cached styled text: [`STYLE_MARKER`]
/// followed by a style digit switches the active font mid-stream, staying
/// inside one type size. `Copy`, so callers can checkpoint before a word
/// and roll back on overflow.
#[derive(Clone, Copy)]
pub struct StyledInkCursor {
    ink: InkCursor,
    size: FontSize,
    font: &'static BitmapFont,
}

impl StyledInkCursor {
    pub fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            ink: InkCursor::new(),
            size: settings.size,
            font: literata_sized(settings.size, default_style),
        }
    }

    /// A [`STYLE_MARKER`] and its code digit must arrive within one
    /// fragment; a marker split across fragments loses its style switch.
    pub fn push_str(&mut self, text: &str) {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    self.font = literata_sized(
                        self.size,
                        style_from_marker_code(code).unwrap_or(FontStyle::Regular),
                    );
                }
                continue;
            }
            self.ink.push_char(self.font, ch);
        }
    }

    pub fn width(&self) -> i16 {
        self.ink.width()
    }
}

pub fn styled_text_ink_width(text: &str, settings: TypeSettings, default_style: FontStyle) -> i16 {
    let mut cursor = StyledInkCursor::new(settings, default_style);
    cursor.push_str(text);
    cursor.width()
}

pub fn first_styled_line_style(text: &str) -> Option<FontStyle> {
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == STYLE_MARKER {
            return chars.next().and_then(style_from_marker_code);
        }
    }
    None
}

/// Greedy word wrap step. Starting at `cursor` (skipping leading ASCII
/// whitespace), returns `(line_start, line_end, next_cursor)` for the
/// longest run of words whose ink width fits `x..max_x`, or a single
/// overlong word when nothing fits.
pub fn next_wrapped_line(
    text: &str,
    mut cursor: usize,
    font: &'static BitmapFont,
    x: i16,
    max_x: i16,
) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }

    let mut ink = InkCursor::new();
    let mut measured_to = cursor;
    let mut scan = cursor;
    let mut best_end = cursor;
    let mut best_next = cursor;
    while scan < bytes.len() {
        let word_start = scan;
        while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        let word_end = scan;
        for ch in text[measured_to..word_end].chars() {
            ink.push_char(font, ch);
        }
        measured_to = word_end;
        if x + ink.width() + READER_WRAP_SAFETY > max_x {
            if best_end == cursor {
                return Some((word_start, word_end, word_end));
            }
            return Some((cursor, best_end, best_next));
        }
        best_end = word_end;
        while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        best_next = scan;
    }

    Some((cursor, best_end, best_next.max(best_end)))
}

pub fn wrapped_line_count(font: &'static BitmapFont, text: &str, max_width: i16) -> u16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut lines = 0u16;
    while let Some((_, _, next_cursor)) = next_wrapped_line(text, cursor, font, 0, max_width) {
        lines = lines.saturating_add(1);
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }
    lines
}

pub fn wrapped_block_height(
    font: &'static BitmapFont,
    text: &str,
    role: TextRole,
    align: TextAlign,
    line_advance: i16,
) -> i16 {
    let max_width = READER_RIGHT_X
        - if align == TextAlign::Center {
            READER_LEFT_X
        } else {
            reader_x_for(role)
        };
    wrapped_line_count(font, text, max_width).max(1) as i16 * line_advance
}

pub fn draw_styled_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    text: &str,
    x: i16,
    baseline_y: i16,
    default_style: FontStyle,
) -> i16 {
    let mut cursor_x = x;
    let mut run_start = 0usize;
    let mut style = default_style;
    let mut iter = text.char_indices().peekable();
    while let Some((index, ch)) = iter.next() {
        if ch != STYLE_MARKER {
            continue;
        }
        if run_start < index {
            cursor_x = draw_text(
                fb,
                body_font(settings, style),
                &text[run_start..index],
                cursor_x,
                baseline_y,
                false,
            );
        }
        if let Some((code_index, code)) = iter.next() {
            style = style_from_marker_code(code).unwrap_or(style);
            run_start = code_index + code.len_utf8();
        } else {
            run_start = index + ch.len_utf8();
        }
    }
    if run_start < text.len() {
        cursor_x = draw_text(
            fb,
            body_font(settings, style),
            &text[run_start..],
            cursor_x,
            baseline_y,
            false,
        );
    }
    cursor_x
}

pub fn draw_centered_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    mut baseline_y: i16,
    max_width: i16,
    line_advance: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, 0, max_width)
    {
        let line = &text[line_start..line_end];
        let width = text_ink_width(font, line).min(max_width);
        let x = ((display::WIDTH as i16 - width) / 2).max(20);
        draw_text(fb, font, line, x, baseline_y, false);
        baseline_y += line_advance;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

pub fn draw_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, x, max_x)
    {
        draw_text(fb, font, &text[line_start..line_end], x, baseline_y, false);
        baseline_y += line_advance;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

pub fn draw_justified_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, x, max_x)
    {
        let is_last_line = next_cursor >= bytes.len();
        draw_justified_line(
            fb,
            font,
            &text[line_start..line_end],
            x,
            baseline_y,
            max_x,
            is_last_line,
        );
        baseline_y += line_advance;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

fn draw_justified_line(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    line: &str,
    x: i16,
    baseline_y: i16,
    max_x: i16,
    is_last_line: bool,
) {
    let gap_count = line
        .as_bytes()
        .windows(2)
        .filter(|pair| pair[0] == b' ' && pair[1] != b' ')
        .count();
    if is_last_line || gap_count == 0 {
        draw_text(fb, font, line, x, baseline_y, false);
        return;
    }

    let text_width = text_ink_width(font, line);
    let extra = (max_x - x - READER_WRAP_SAFETY - text_width).max(0);
    let extra_per_gap = extra / gap_count as i16;
    let mut remainder = extra % gap_count as i16;
    let mut cursor_x = x;
    let mut word_start = None;

    for (index, byte) in line.bytes().enumerate() {
        if byte == b' ' {
            if let Some(start) = word_start.take() {
                let word = &line[start..index];
                cursor_x = draw_text(fb, font, word, cursor_x, baseline_y, false);
            }
            let mut gap = measure_text(font, " ") as i16 + extra_per_gap;
            if remainder > 0 {
                gap += 1;
                remainder -= 1;
            }
            cursor_x += gap;
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        draw_text(fb, font, &line[start..], cursor_x, baseline_y, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use display::font::style_marker_code;

    /// Reference implementation: measure the whole string from scratch,
    /// exactly as the pre-incremental firmware code did.
    fn naive_text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
        let mut advance = 0i16;
        let mut right = 0i16;
        for ch in text.chars() {
            let codepoint = if ch as u32 > u16::MAX as u32 {
                b'?' as u16
            } else {
                ch as u16
            };
            let Some((metric, _)) = font.glyph(codepoint).or_else(|| font.glyph(b'?' as u16))
            else {
                advance += 8;
                right = right.max(advance);
                continue;
            };
            let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
            right = right.max(glyph_right);
            advance += metric.advance as i16;
        }
        right.max(advance)
    }

    /// Reference wrap: re-measures every candidate line per word, exactly
    /// as the pre-incremental firmware code did.
    fn naive_next_wrapped_line(
        text: &str,
        mut cursor: usize,
        font: &'static BitmapFont,
        x: i16,
        max_x: i16,
    ) -> Option<(usize, usize, usize)> {
        let bytes = text.as_bytes();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }

        let mut scan = cursor;
        let mut best_end = cursor;
        let mut best_next = cursor;
        while scan < bytes.len() {
            let word_start = scan;
            while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            let word_end = scan;
            let candidate = &text[cursor..word_end];
            if x + naive_text_ink_width(font, candidate) + READER_WRAP_SAFETY > max_x {
                if best_end == cursor {
                    return Some((word_start, word_end, word_end));
                }
                return Some((cursor, best_end, best_next));
            }
            best_end = word_end;
            while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            best_next = scan;
        }

        Some((cursor, best_end, best_next.max(best_end)))
    }

    const SAMPLES: &[&str] = &[
        "",
        " ",
        "word",
        "two words",
        "It was the best of times, it was the worst of times, it was the age of wisdom.",
        "  leading and   irregular   spacing between words  ",
        "Supercalifragilisticexpialidocious-antidisestablishmentarianism-longword",
        "short a b c d e f g h i j k l m n o p q r s t u v w x y z",
        "punctuation, everywhere! (parentheses) \"quotes\" -- dashes; colons: done.",
        "tabs\tand\nnewlines\ras whitespace",
        "non-latin \u{4e16}\u{754c} mixed with latin text and \u{20ac} symbols",
        "beyond bmp \u{1F600} falls back to question mark",
    ];

    const STYLES: [FontStyle; 4] = [
        FontStyle::Regular,
        FontStyle::Italic,
        FontStyle::Bold,
        FontStyle::BoldItalic,
    ];

    const ALL_SETTINGS: [TypeSettings; 9] = {
        let sizes = [FontSize::Small, FontSize::Medium, FontSize::Large];
        let spacings = [
            LineSpacing::Compact,
            LineSpacing::Normal,
            LineSpacing::Relaxed,
        ];
        let mut out = [TypeSettings::DEFAULT; 9];
        let mut i = 0;
        while i < 3 {
            let mut j = 0;
            while j < 3 {
                out[i * 3 + j] = TypeSettings {
                    size: sizes[i],
                    spacing: spacings[j],
                };
                j += 1;
            }
            i += 1;
        }
        out
    };

    fn fonts() -> [&'static BitmapFont; 4] {
        STYLES.map(|style| body_font(TypeSettings::DEFAULT, style))
    }

    #[test]
    fn ink_width_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                assert_eq!(
                    text_ink_width(font, sample),
                    naive_text_ink_width(font, sample),
                    "sample {sample:?}"
                );
            }
        }
    }

    #[test]
    fn next_wrapped_line_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                for max_x in [40i16, 120, 300, 784] {
                    for x in [0i16, 8, 32] {
                        let mut cursor = 0usize;
                        loop {
                            let fast = next_wrapped_line(sample, cursor, font, x, max_x);
                            let slow = naive_next_wrapped_line(sample, cursor, font, x, max_x);
                            assert_eq!(fast, slow, "sample {sample:?} x {x} max_x {max_x}");
                            let Some((_, _, next_cursor)) = fast else {
                                break;
                            };
                            assert!(next_cursor > cursor, "wrap must make progress");
                            cursor = next_cursor;
                            if cursor >= sample.len() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn styled_width_matches_unstyled_when_unmarked() {
        for settings in ALL_SETTINGS {
            for style in STYLES {
                for sample in SAMPLES {
                    assert_eq!(
                        styled_text_ink_width(sample, settings, style),
                        naive_text_ink_width(body_font(settings, style), sample),
                        "sample {sample:?} settings {settings:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_configs_are_distinct_per_type_settings() {
        let mut seen = [0u16; 9];
        for (index, settings) in ALL_SETTINGS.iter().enumerate() {
            let config = reader_layout_config(*settings);
            assert!(
                !seen[..index].contains(&config),
                "duplicate layout config {config} for {settings:?}"
            );
            seen[index] = config;
        }
    }

    #[test]
    fn line_advances_grow_with_size_and_spacing() {
        for role in [TextRole::Body, TextRole::Heading1] {
            for window in ALL_SETTINGS.windows(2) {
                assert!(
                    line_advance(window[0], role) < line_advance(window[1], role)
                        || window[0].size != window[1].size,
                    "advance must grow with spacing within a size: {window:?}"
                );
            }
        }
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Body), 27);
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Heading1), 32);
    }

    #[test]
    fn styled_cursor_checkpoint_equals_full_measure() {
        let mut styled = heapless::String::<256>::new();
        let _ = styled.push_str("plain ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Italic));
        let _ = styled.push_str("slanted words ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Bold));
        let _ = styled.push_str("heavy end");

        // Push in arbitrary fragments; the running width must match a
        // one-shot measure at every fragment boundary, at every size.
        for settings in ALL_SETTINGS {
            let text = styled.as_str();
            let mut cursor = StyledInkCursor::new(settings, FontStyle::Regular);
            let mut consumed = 0usize;
            for chunk in [3usize, 1, 9, 2, 30, 200] {
                let end = (consumed + chunk).min(text.len());
                while !text.is_char_boundary(consumed.min(text.len())) {
                    consumed += 1;
                }
                let mut safe_end = end;
                while safe_end < text.len() && !text.is_char_boundary(safe_end) {
                    safe_end += 1;
                }
                cursor.push_str(&text[consumed..safe_end]);
                consumed = safe_end;
                assert_eq!(
                    cursor.width(),
                    styled_text_ink_width(&text[..consumed], settings, FontStyle::Regular),
                    "prefix {:?} settings {settings:?}",
                    &text[..consumed]
                );
                if consumed >= text.len() {
                    break;
                }
            }
        }
    }
}
