use crate::reader_store::ReaderStore;
use display::fb::Framebuffer;
use display::font::{
    fixed_ceil, fixed_round, FontSize, FontStyle, FontWeight, GlyphMetric, STYLE_MARKER,
};
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use proto::cache::PageRecord;
use proto::font_pack::{
    font_pack_codepoint_index, FontPackFaceRecord, FONT_PACK_DIR, FONT_PACK_FACE_BOLD,
    FONT_PACK_FACE_BOLD_ITALIC, FONT_PACK_FACE_ITALIC, FONT_PACK_FACE_REGULAR, FONT_PACK_FILE,
    FONT_PACK_METRIC_BYTES, FONT_PACK_SIZE_LARGE, FONT_PACK_SIZE_MEDIUM, FONT_PACK_SIZE_SMALL,
};
use proto::text::TextAlign;

const MAX_ROW_BYTES: usize = 32;

/// Printable ASCII is the first codepoint range of the pack
/// (`font_pack_codepoint_index(0x20) == 0`), so a face's ASCII metrics are
/// one contiguous run at the start of its metric table.
const ASCII_METRIC_COUNT: usize = 95;
const ASCII_FIRST: u16 = 0x20;
const ASCII_LAST: u16 = 0x7E;
/// Regular/Italic/Bold/BoldItalic of the active size all stay resident.
const METRIC_CACHE_SLOTS: usize = 4;

/// RAM cache of the printable-ASCII metric rows for custom font faces.
///
/// Line measurement during a cold book build calls into the pack once per
/// character of the whole book; without this cache each call was a
/// directory walk, a file open, a seek, and a 12-byte read. A slot fills
/// once per face from an open pack file and then serves the overwhelming
/// majority of characters from RAM; non-ASCII falls through to the
/// per-char read path. Slots are keyed by pack identity plus the face's
/// metric-table offset (unique per size and style within a pack), so a
/// changed pack or size misses and refills naturally.
pub(crate) struct MetricCache {
    slots: [MetricSlot; METRIC_CACHE_SLOTS],
    next_evict: usize,
}

#[derive(Clone, Copy)]
struct MetricSlot {
    key: Option<(u64, u32)>,
    records: [[u8; FONT_PACK_METRIC_BYTES]; ASCII_METRIC_COUNT],
}

const EMPTY_METRIC_SLOT: MetricSlot = MetricSlot {
    key: None,
    records: [[0u8; FONT_PACK_METRIC_BYTES]; ASCII_METRIC_COUNT],
};

impl MetricCache {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [EMPTY_METRIC_SLOT; METRIC_CACHE_SLOTS],
            next_evict: 0,
        }
    }

    fn ascii_index(ch: char) -> Option<usize> {
        let code = ch as u32;
        (code >= ASCII_FIRST as u32 && code <= ASCII_LAST as u32)
            .then(|| (code - ASCII_FIRST as u32) as usize)
    }

    /// Serve a cached ASCII metric without touching the card. `None` means
    /// the caller must open the pack: non-ASCII char or unfilled face.
    fn lookup(&self, identity: u64, face: FontPackFaceRecord, ch: char) -> Option<GlyphMetric> {
        let index = Self::ascii_index(ch)?;
        let key = (identity, face.metrics_offset);
        self.slots
            .iter()
            .find(|slot| slot.key == Some(key))
            .map(|slot| decode_metric(&slot.records[index]))
    }

    /// Fill the face's slot from an open pack file (one sequential sweep of
    /// its ASCII run), then serve the char. Non-ASCII and packs without the
    /// full ASCII range fall back to the per-char read.
    fn fill_and_lookup<
        D,
        T,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    >(
        &mut self,
        file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        identity: u64,
        face: FontPackFaceRecord,
        ch: char,
    ) -> Option<GlyphMetric>
    where
        D: embedded_sdmmc::BlockDevice,
        T: TimeSource,
    {
        let Some(index) = Self::ascii_index(ch) else {
            return metric_for_char(file, face, ch);
        };
        let key = (identity, face.metrics_offset);
        if let Some(slot) = self.slots.iter().find(|slot| slot.key == Some(key)) {
            return Some(decode_metric(&slot.records[index]));
        }
        if (face.metric_count as usize) < ASCII_METRIC_COUNT {
            return metric_for_char(file, face, ch);
        }
        let slot_index = self
            .slots
            .iter()
            .position(|slot| slot.key.is_none())
            .unwrap_or_else(|| {
                let evict = self.next_evict;
                self.next_evict = (self.next_evict + 1) % METRIC_CACHE_SLOTS;
                evict
            });
        let slot = &mut self.slots[slot_index];
        slot.key = None;
        file.seek_from_start(face.metrics_offset).ok()?;
        for record in slot.records.iter_mut() {
            read_full(file, record)?;
        }
        slot.key = Some(key);
        Some(decode_metric(&slot.records[index]))
    }
}

pub(crate) fn draw_reading_page_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    fb: &mut Framebuffer,
    source: &ReaderStore,
    cache: &mut MetricCache,
    page: PageRecord,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !source.custom_font_available() {
        return false;
    }
    let Some(xteink) = root.open_dir(proto::cache::CACHE_ROOT_DIR).ok() else {
        return false;
    };
    let Some(fonts) = xteink.open_dir(FONT_PACK_DIR).ok() else {
        return false;
    };
    let Some(file) = fonts.open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly).ok() else {
        return false;
    };
    let mut font = CustomFontFile {
        file: &file,
        source,
        cache,
    };
    let settings = source.type_settings();
    let page_box = ui::reading::ReadingBlocks::page_box(source);
    let mut ok = true;
    ui::reading::for_each_drawable_block(source, page, |block| {
        if block.record.line_count != 1 {
            ok = false;
            return false;
        }
        let x = match block.record.align {
            TextAlign::Center => {
                let width = font
                    .styled_ink_width(block.text, settings.size, settings.weight, block.style)
                    .min(page_box.right - page_box.left);
                ((page_box.left + page_box.right - width) / 2).max(page_box.left)
            }
            TextAlign::Left | TextAlign::Justify => {
                page_box.x_for(block.record.role) + block.indent
            }
        };
        if !font.draw_styled_line(
            fb,
            block.text,
            settings.size,
            settings.weight,
            block.style,
            x,
            block.y,
        ) {
            ok = false;
            return false;
        }
        true
    });
    ok
}

pub(crate) fn measure_char<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source: &ReaderStore,
    cache: &mut MetricCache,
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
    ch: char,
) -> Option<GlyphMetric>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let face = custom_face(source, size, weight, style)?;
    let identity = source.custom_font_identity();
    if let Some(metric) = cache.lookup(identity, face, ch) {
        return Some(metric);
    }
    // Cache miss (unfilled face or non-ASCII): open the pack once for this
    // char; an ASCII miss fills the face's whole slot while it's open.
    let xteink = root.open_dir(proto::cache::CACHE_ROOT_DIR).ok()?;
    let fonts = xteink.open_dir(FONT_PACK_DIR).ok()?;
    let file = fonts
        .open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly)
        .ok()?;
    cache.fill_and_lookup(&file, identity, face, ch)
}

struct CustomFontFile<
    'file,
    'volume,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'file File<'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source: &'file ReaderStore,
    cache: &'file mut MetricCache,
}

impl<
        'file,
        'volume,
        D,
        T,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > CustomFontFile<'file, 'volume, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[allow(clippy::too_many_arguments)]
    fn draw_styled_line(
        &mut self,
        fb: &mut Framebuffer,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
        x: i16,
        baseline_y: i16,
    ) -> bool {
        let mut cursor_x = x;
        let mut run_start = 0usize;
        let mut style = default_style;
        let mut iter = text.char_indices().peekable();
        while let Some((index, ch)) = iter.next() {
            if ch != STYLE_MARKER {
                continue;
            }
            if run_start < index {
                let Some(next_x) = self.draw_text(
                    fb,
                    &text[run_start..index],
                    size,
                    weight,
                    style,
                    cursor_x,
                    baseline_y,
                ) else {
                    return false;
                };
                cursor_x = next_x;
            }
            if let Some((code_index, code)) = iter.next() {
                style = display::font::style_from_marker_code(code).unwrap_or(style);
                run_start = code_index + code.len_utf8();
            } else {
                run_start = index + ch.len_utf8();
            }
        }
        if run_start < text.len() {
            self.draw_text(
                fb,
                &text[run_start..],
                size,
                weight,
                style,
                cursor_x,
                baseline_y,
            )
            .is_some()
        } else {
            true
        }
    }

    fn styled_ink_width(
        &mut self,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        default_style: FontStyle,
    ) -> i16 {
        let mut ink = CustomInkCursor::new();
        let mut style = default_style;
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    ink.reset_pair();
                    style = display::font::style_from_marker_code(code).unwrap_or(style);
                }
                continue;
            }
            if let Some(face) = custom_face(self.source, size, weight, style) {
                ink.push_char(self, face, ch);
            }
        }
        ink.width()
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        fb: &mut Framebuffer,
        text: &str,
        size: FontSize,
        weight: FontWeight,
        style: FontStyle,
        x: i16,
        baseline_y: i16,
    ) -> Option<i16> {
        let face = custom_face(self.source, size, weight, style)?;
        let mut cursor_fp = (x as i32) << 4;
        for ch in text.chars() {
            let x = fixed_round(cursor_fp);
            let metric = self.metric_for_char(face, ch)?;
            self.draw_glyph_bitmap(fb, face, metric, x, baseline_y)?;
            cursor_fp += metric.advance_fp as i32;
        }
        Some(fixed_round(cursor_fp))
    }

    fn metric_for_char(&mut self, face: FontPackFaceRecord, ch: char) -> Option<GlyphMetric> {
        let identity = self.source.custom_font_identity();
        if let Some(metric) = self.cache.lookup(identity, face, ch) {
            return Some(metric);
        }
        self.cache.fill_and_lookup(self.file, identity, face, ch)
    }

    fn draw_glyph_bitmap(
        &self,
        fb: &mut Framebuffer,
        face: FontPackFaceRecord,
        metric: GlyphMetric,
        x: i16,
        baseline_y: i16,
    ) -> Option<()> {
        let row_bytes = (metric.width as usize).div_ceil(8);
        if row_bytes > MAX_ROW_BYTES {
            return None;
        }
        let glyph_x = x + metric.x_offset as i16;
        let glyph_y = baseline_y + metric.y_offset as i16;
        let mut row = [0u8; MAX_ROW_BYTES];
        for y in 0..metric.height as usize {
            let offset = face
                .bitmap_offset
                .checked_add(metric.offset)?
                .checked_add((y * row_bytes) as u32)?;
            self.file.seek_from_start(offset).ok()?;
            read_full(self.file, &mut row[..row_bytes])?;
            for (x_byte, &byte) in row.iter().enumerate().take(row_bytes) {
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
                            fb.set_pixel(draw_x as usize, draw_y as usize, false);
                        }
                    }
                }
            }
        }
        Some(())
    }
}

fn custom_face(
    source: &ReaderStore,
    size: FontSize,
    weight: FontWeight,
    style: FontStyle,
) -> Option<FontPackFaceRecord> {
    let size_px = match size {
        FontSize::Small => FONT_PACK_SIZE_SMALL,
        FontSize::Medium => FONT_PACK_SIZE_MEDIUM,
        FontSize::Large => FONT_PACK_SIZE_LARGE,
    };
    let style = match (weight, style) {
        (FontWeight::Normal, FontStyle::Regular) => FONT_PACK_FACE_REGULAR,
        (FontWeight::Normal, FontStyle::Italic) => FONT_PACK_FACE_ITALIC,
        (FontWeight::Normal, FontStyle::Bold) => FONT_PACK_FACE_BOLD,
        (FontWeight::Normal, FontStyle::BoldItalic) => FONT_PACK_FACE_BOLD_ITALIC,
        (FontWeight::Heavy, FontStyle::Regular | FontStyle::Bold) => FONT_PACK_FACE_BOLD,
        (FontWeight::Heavy, FontStyle::Italic | FontStyle::BoldItalic) => {
            FONT_PACK_FACE_BOLD_ITALIC
        }
    };
    source.custom_font_face(size_px, style)
}

fn metric_for_char<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    face: FontPackFaceRecord,
    ch: char,
) -> Option<GlyphMetric>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let codepoint = if ch as u32 > u16::MAX as u32 {
        b'?' as u16
    } else {
        ch as u16
    };
    metric(file, face, codepoint).or_else(|| metric(file, face, b'?' as u16))
}

fn metric<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    face: FontPackFaceRecord,
    codepoint: u16,
) -> Option<GlyphMetric>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let index = font_pack_codepoint_index(codepoint)?;
    if index >= face.metric_count as usize {
        return None;
    }
    let offset = face
        .metrics_offset
        .checked_add((index * FONT_PACK_METRIC_BYTES) as u32)?;
    let mut bytes = [0u8; FONT_PACK_METRIC_BYTES];
    file.seek_from_start(offset).ok()?;
    read_full(file, &mut bytes)?;
    Some(decode_metric(&bytes))
}

fn decode_metric(bytes: &[u8; FONT_PACK_METRIC_BYTES]) -> GlyphMetric {
    GlyphMetric {
        offset: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        len: u16::from_le_bytes([bytes[4], bytes[5]]),
        width: bytes[6],
        height: bytes[7],
        x_offset: bytes[8] as i8,
        y_offset: bytes[9] as i8,
        advance_fp: u16::from_le_bytes([bytes[10], bytes[11]]),
    }
}

fn read_full<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut buf: &mut [u8],
) -> Option<()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !buf.is_empty() {
        let read = file.read(buf).ok()?;
        if read == 0 {
            return None;
        }
        let tmp = buf;
        buf = &mut tmp[read..];
    }
    Some(())
}

#[derive(Clone, Copy, Debug, Default)]
struct CustomInkCursor {
    advance_fp: i32,
    right: i16,
}

impl CustomInkCursor {
    const fn new() -> Self {
        Self {
            advance_fp: 0,
            right: 0,
        }
    }

    fn push_char<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
        &mut self,
        font: &mut CustomFontFile<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        face: FontPackFaceRecord,
        ch: char,
    ) where
        D: embedded_sdmmc::BlockDevice,
        T: TimeSource,
    {
        let Some(metric) = font.metric_for_char(face, ch) else {
            self.advance_fp += 8 << 4;
            self.right = self.right.max(fixed_ceil(self.advance_fp));
            return;
        };
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
    }

    fn reset_pair(&mut self) {}

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}
