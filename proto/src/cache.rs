use crate::text::{FontStyle, TextAlign, TextRole};
use heapless::String;

pub const CACHE_MAGIC: u32 = 0x5834_5244; // X4RD
pub const CACHE_VERSION: u16 = 1;
// Bumped 21 -> 23 with the spine-cap fix. A long book cached under the old
// 96-item spine cap was written with partial=false (truncation never tripped
// book_partial), so it would load as a clean hit and keep stranding the tail
// chapters on patched firmware. Rejecting the old versions forces a one-time,
// lazy, per-book re-paginate; surviving chapters lay out identically, so
// chapter-keyed positions carry over.
//
// Bumped 23 -> 24 with the EPUB 3 nav page-list fix. Bumped 24 -> 25 when the
// nav classifier widened beyond `epub:type` to also reject page lists,
// landmarks, lists of illustrations/tables, and back-matter index navs marked
// by role/label/id/class. A book already indexed on buggy firmware loads via
// the fast book-cache path, which never re-parses the nav or rewrites TOC.BIN,
// so the bogus chapter list would persist. Rejecting v24 forces the one-time
// rebuild that re-runs the corrected nav parser; the re-paginate is incidental
// and chapter-keyed positions carry over.
//
// Bumped 25 -> 26 when custom font identity joined v2 book/section headers.
// A custom pack replacement can keep the same FontFamily::Custom setting while
// changing metrics, so the cache key needs the pack hash too.
pub const CACHE_V2_VERSION: u16 = 26;
const CACHE_V2_COMPAT_VERSION: u16 = 26;
pub const CACHE_ROOT_DIR: &str = "XTEINK";
pub const CACHE_DIR: &str = "CACHE";
pub const CACHE_V2_DIR: &str = "CACHE2";
pub const CACHE_SECTIONS_DIR: &str = "SECTIONS";
pub const CACHE_BOOK_FILE: &str = "BOOK.BIN";
pub const CACHE_COVER_FILE: &str = "COVER.BIN";
pub const CACHE_STATE_FILE: &str = "STATE.BIN";
pub const CACHE_KEY_BYTES: usize = 8;
pub const CACHE_SECTION_FILE_BYTES: usize = 8;
pub const BOOK_HEADER_BYTES: usize = 16;
pub const SPINE_RECORD_BYTES: usize = 12;
pub const TOC_RECORD_BYTES: usize = 24;
pub const SECTION_HEADER_BYTES: usize = 40;
pub const SECTION_V2_HEADER_BYTES: usize = 56;
pub const BOOK_V2_HEADER_BYTES: usize = 56;
pub const BOOK_V2_SECTION_RECORD_BYTES: usize = 16;
pub const PAGE_HEADER_BYTES: usize = 28;
pub const PAGE_RECORD_BYTES: usize = 4;
pub const LINE_RECORD_BYTES: usize = 12;
pub const WORD_RECORD_BYTES: usize = 12;
pub const BLOCK_RECORD_BYTES: usize = 12;
pub const COVER_MAGIC: &[u8; 4] = b"X4CV";
pub const COVER_VERSION: u8 = 1;
pub const COVER_HEADER_BYTES: usize = 12;
pub const COVER_WIDTH: usize = 202;
pub const COVER_HEIGHT: usize = 303;
pub const COVER_STRIDE: usize = COVER_WIDTH.div_ceil(8);
pub const COVER_BYTES: usize = COVER_STRIDE * COVER_HEIGHT;
/// Chapter list (TOC) cache, kept on disk so the full table of contents
/// never has to be resident -- a long book's TOC (HPMOR's runs to a couple
/// hundred entries) would otherwise blow the tight reader RAM budget. Fixed
/// 48-byte records keep it randomly addressable: chapter `i` lives at
/// `TOC_FILE_HEADER_BYTES + i * TOC_CHAPTER_RECORD_BYTES`.
pub const CACHE_TOC_FILE: &str = "TOC.BIN";
pub const TOC_FILE_MAGIC: u32 = 0x5834_5443; // X4TC
                                             // v2: chapter title budget grew 44->60 bytes (record 48->64). 64-byte records
                                             // keep MAX_OVERVIEW_CHAPTERS (256) fitting the 16KB overview text buffer exactly
                                             // (256*64 == 16384). A v1 TOC.BIN is rejected here and rebuilt (chapter-list
                                             // re-parse only, no re-pagination).
pub const TOC_FILE_VERSION: u16 = 2;
pub const TOC_FILE_HEADER_BYTES: usize = 16;
pub const TOC_CHAPTER_TITLE_BYTES: usize = 60;
pub const TOC_CHAPTER_RECORD_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheError {
    BufferTooSmall,
    BadMagic,
    BadVersion,
    BadLength,
    Utf8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookCacheHeader {
    pub spine_count: u16,
    pub toc_count: u16,
    pub string_bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpineRecord {
    pub href_offset: u32,
    pub href_len: u16,
    pub toc_index: i16,
    pub byte_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TocRecord {
    pub title_offset: u32,
    pub title_len: u16,
    pub href_offset: u32,
    pub href_len: u16,
    pub anchor_offset: u32,
    pub anchor_len: u16,
    pub level: u8,
    pub spine_index: i16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    pub page_count: u16,
    pub block_count: u16,
    pub line_count: u16,
    pub word_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub bytes_consumed: u32,
    pub total_bytes: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionV2Header {
    pub source_hash: u32,
    pub source_size: u32,
    pub spine: u16,
    pub page_count: u16,
    pub block_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub custom_font_identity: u64,
    pub bytes_consumed: u32,
    pub total_bytes: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookV2Header {
    pub source_hash: u32,
    pub source_size: u32,
    pub total_pages: u32,
    pub section_count: u16,
    pub spine_count: u16,
    pub toc_count: u16,
    pub toc_text_bytes: u32,
    pub title_text_bytes: u32,
    pub author_text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub custom_font_identity: u64,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookV2SectionRecord {
    pub section: u16,
    pub spine: u16,
    pub start_page: u32,
    pub page_count: u16,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCacheHeader {
    pub page_count: u16,
    pub block_count: u16,
    pub text_bytes: u32,
    pub viewport_width: u16,
    pub viewport_height: u16,
    pub font_config: u16,
    pub bytes_consumed: u32,
    pub partial: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRecord {
    pub first_block: u16,
    pub block_count: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRecord {
    pub first_word: u16,
    pub word_count: u16,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub align: TextAlign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordRecord {
    pub text_offset: u32,
    pub text_len: u16,
    pub x: i16,
    pub width: u16,
    pub style: FontStyle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockRecord {
    pub text_offset: u32,
    pub text_len: u16,
    pub line_count: u8,
    pub role: TextRole,
    pub style: FontStyle,
    pub align: TextAlign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverCacheHeader {
    pub width: u16,
    pub height: u16,
    pub stride: u16,
}

impl CoverCacheHeader {
    pub const fn x4_dock_clean() -> Self {
        Self {
            width: COVER_WIDTH as u16,
            height: COVER_HEIGHT as u16,
            stride: COVER_STRIDE as u16,
        }
    }
}

pub fn book_cache_size(header: BookCacheHeader) -> usize {
    BOOK_HEADER_BYTES
        + header.spine_count as usize * SPINE_RECORD_BYTES
        + header.toc_count as usize * TOC_RECORD_BYTES
        + header.string_bytes as usize
}

pub fn page_cache_size(header: PageCacheHeader) -> usize {
    PAGE_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.text_bytes as usize
}

pub fn section_cache_size(header: SectionHeader) -> usize {
    SECTION_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.block_count as usize
        + header.line_count as usize * LINE_RECORD_BYTES
        + header.word_count as usize * WORD_RECORD_BYTES
        + header.text_bytes as usize
}

pub fn section_v2_cache_size(header: SectionV2Header) -> usize {
    SECTION_V2_HEADER_BYTES
        + header.page_count as usize * PAGE_RECORD_BYTES
        + header.block_count as usize * BLOCK_RECORD_BYTES
        + header.block_count as usize
        + header.text_bytes as usize
}

pub fn book_v2_cache_size(header: BookV2Header) -> usize {
    BOOK_V2_HEADER_BYTES
        + header.section_count as usize * BOOK_V2_SECTION_RECORD_BYTES
        + header.toc_count as usize * TOC_RECORD_BYTES
        + header.toc_text_bytes as usize
        + header.title_text_bytes as usize
        + header.author_text_bytes as usize
}

pub fn cache_key_for(source_path: &str, source_len: u32) -> String<CACHE_KEY_BYTES> {
    let mut hash = 0x811c_9dc5u32;
    for byte in source_path.bytes().chain(source_len.to_le_bytes()) {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let mut out = String::<CACHE_KEY_BYTES>::new();
    let _ = out.push('E');
    push_hex(&mut out, hash & 0x0FFF_FFFF, 7);
    out
}

pub fn section_file_name<const N: usize>(spine: u16, out: &mut String<N>) {
    out.clear();
    let _ = out.push('S');
    push_dec3(out, spine);
    let _ = out.push_str(".BIN");
}

pub fn encode_book_header(header: BookCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BOOK_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.spine_count);
    write_u16(out, 8, header.toc_count);
    write_u16(out, 10, 0);
    write_u32(out, 12, header.string_bytes);
    Ok(BOOK_HEADER_BYTES)
}

pub fn decode_book_header(input: &[u8]) -> Result<BookCacheHeader, CacheError> {
    require(input, BOOK_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(BookCacheHeader {
        spine_count: read_u16(input, 6)?,
        toc_count: read_u16(input, 8)?,
        string_bytes: read_u32(input, 12)?,
    })
}

pub fn encode_spine(record: SpineRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, SPINE_RECORD_BYTES)?;
    write_u32(out, 0, record.href_offset);
    write_u16(out, 4, record.href_len);
    write_i16(out, 6, record.toc_index);
    write_u32(out, 8, record.byte_size);
    Ok(SPINE_RECORD_BYTES)
}

pub fn decode_spine(input: &[u8]) -> Result<SpineRecord, CacheError> {
    require(input, SPINE_RECORD_BYTES)?;
    Ok(SpineRecord {
        href_offset: read_u32(input, 0)?,
        href_len: read_u16(input, 4)?,
        toc_index: read_i16(input, 6)?,
        byte_size: read_u32(input, 8)?,
    })
}

pub fn encode_toc(record: TocRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_RECORD_BYTES)?;
    write_u32(out, 0, record.title_offset);
    write_u16(out, 4, record.title_len);
    write_u16(out, 6, 0);
    write_u32(out, 8, record.href_offset);
    write_u16(out, 12, record.href_len);
    write_u16(out, 14, 0);
    write_u32(out, 16, record.anchor_offset);
    write_u16(out, 20, record.anchor_len);
    out[22] = record.level;
    out[23] = 0;
    write_i16(out, 14, record.spine_index);
    Ok(TOC_RECORD_BYTES)
}

pub fn decode_toc(input: &[u8]) -> Result<TocRecord, CacheError> {
    require(input, TOC_RECORD_BYTES)?;
    Ok(TocRecord {
        title_offset: read_u32(input, 0)?,
        title_len: read_u16(input, 4)?,
        href_offset: read_u32(input, 8)?,
        href_len: read_u16(input, 12)?,
        anchor_offset: read_u32(input, 16)?,
        anchor_len: read_u16(input, 20)?,
        level: input[22],
        spine_index: read_i16(input, 14)?,
    })
}

pub fn encode_page_header(header: PageCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, PAGE_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.page_count);
    write_u16(out, 8, header.block_count);
    out[10] = header.partial as u8;
    out[11] = 0;
    write_u32(out, 12, header.text_bytes);
    write_u16(out, 16, header.viewport_width);
    write_u16(out, 18, header.viewport_height);
    write_u16(out, 20, header.font_config);
    write_u16(out, 22, 0);
    write_u32(out, 24, header.bytes_consumed);
    Ok(PAGE_HEADER_BYTES)
}

pub fn encode_section_header(header: SectionHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, SECTION_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_VERSION);
    write_u16(out, 6, header.page_count);
    write_u16(out, 8, header.block_count);
    write_u16(out, 10, header.line_count);
    write_u16(out, 12, header.word_count);
    out[14] = header.partial as u8;
    out[15] = 0;
    write_u32(out, 16, header.text_bytes);
    write_u16(out, 20, header.viewport_width);
    write_u16(out, 22, header.viewport_height);
    write_u16(out, 24, header.font_config);
    write_u16(out, 26, 0);
    write_u32(out, 28, header.bytes_consumed);
    write_u32(out, 32, header.total_bytes);
    write_u32(out, 36, 0);
    Ok(SECTION_HEADER_BYTES)
}

pub fn decode_section_header(input: &[u8]) -> Result<SectionHeader, CacheError> {
    require(input, SECTION_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(SectionHeader {
        page_count: read_u16(input, 6)?,
        block_count: read_u16(input, 8)?,
        line_count: read_u16(input, 10)?,
        word_count: read_u16(input, 12)?,
        partial: input[14] != 0,
        text_bytes: read_u32(input, 16)?,
        viewport_width: read_u16(input, 20)?,
        viewport_height: read_u16(input, 22)?,
        font_config: read_u16(input, 24)?,
        bytes_consumed: read_u32(input, 28)?,
        total_bytes: read_u32(input, 32)?,
    })
}

pub fn encode_section_v2_header(
    header: SectionV2Header,
    out: &mut [u8],
) -> Result<usize, CacheError> {
    require(out, SECTION_V2_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_V2_VERSION);
    write_u16(out, 6, header.spine);
    write_u16(out, 8, header.page_count);
    write_u16(out, 10, header.block_count);
    out[12] = header.partial as u8;
    out[13] = 0;
    write_u16(out, 14, 0);
    write_u32(out, 16, header.text_bytes);
    write_u16(out, 20, header.viewport_width);
    write_u16(out, 22, header.viewport_height);
    write_u16(out, 24, header.font_config);
    write_u16(out, 26, 0);
    write_u32(out, 28, header.bytes_consumed);
    write_u32(out, 32, header.total_bytes);
    write_u32(out, 36, header.source_hash);
    write_u32(out, 40, header.source_size);
    write_u64(out, 44, header.custom_font_identity);
    write_u32(out, 52, 0);
    Ok(SECTION_V2_HEADER_BYTES)
}

pub fn decode_section_v2_header(input: &[u8]) -> Result<SectionV2Header, CacheError> {
    require(input, SECTION_V2_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if !valid_cache_v2_version(read_u16(input, 4)?) {
        return Err(CacheError::BadVersion);
    }
    Ok(SectionV2Header {
        spine: read_u16(input, 6)?,
        page_count: read_u16(input, 8)?,
        block_count: read_u16(input, 10)?,
        partial: input[12] != 0,
        text_bytes: read_u32(input, 16)?,
        viewport_width: read_u16(input, 20)?,
        viewport_height: read_u16(input, 22)?,
        font_config: read_u16(input, 24)?,
        custom_font_identity: read_u64(input, 44)?,
        bytes_consumed: read_u32(input, 28)?,
        total_bytes: read_u32(input, 32)?,
        source_hash: read_u32(input, 36)?,
        source_size: read_u32(input, 40)?,
    })
}

pub fn encode_book_v2_header(header: BookV2Header, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BOOK_V2_HEADER_BYTES)?;
    write_u32(out, 0, CACHE_MAGIC);
    write_u16(out, 4, CACHE_V2_VERSION);
    out[6] = header.partial as u8;
    out[7] = 0;
    write_u32(out, 8, header.source_hash);
    write_u32(out, 12, header.source_size);
    write_u32(out, 16, header.total_pages);
    write_u16(out, 20, header.section_count);
    write_u16(out, 22, header.spine_count);
    write_u16(out, 24, header.toc_count);
    write_u16(out, 26, 0);
    write_u16(out, 28, header.viewport_width);
    write_u16(out, 30, header.viewport_height);
    write_u16(out, 32, header.font_config);
    write_u16(out, 34, 0);
    write_u32(out, 36, header.toc_text_bytes);
    write_u32(out, 40, header.title_text_bytes);
    write_u32(out, 44, header.author_text_bytes);
    write_u64(out, 48, header.custom_font_identity);
    Ok(BOOK_V2_HEADER_BYTES)
}

pub fn decode_book_v2_header(input: &[u8]) -> Result<BookV2Header, CacheError> {
    require(input, BOOK_V2_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if !valid_cache_v2_version(read_u16(input, 4)?) {
        return Err(CacheError::BadVersion);
    }
    Ok(BookV2Header {
        partial: input[6] != 0,
        source_hash: read_u32(input, 8)?,
        source_size: read_u32(input, 12)?,
        total_pages: read_u32(input, 16)?,
        section_count: read_u16(input, 20)?,
        spine_count: read_u16(input, 22)?,
        toc_count: read_u16(input, 24)?,
        toc_text_bytes: read_u32(input, 36)?,
        title_text_bytes: read_u32(input, 40)?,
        author_text_bytes: read_u32(input, 44)?,
        viewport_width: read_u16(input, 28)?,
        viewport_height: read_u16(input, 30)?,
        font_config: read_u16(input, 32)?,
        custom_font_identity: read_u64(input, 48)?,
    })
}

pub fn encode_book_v2_section(
    record: BookV2SectionRecord,
    out: &mut [u8],
) -> Result<usize, CacheError> {
    require(out, BOOK_V2_SECTION_RECORD_BYTES)?;
    write_u16(out, 0, record.section);
    write_u16(out, 2, record.spine);
    write_u32(out, 4, record.start_page);
    write_u16(out, 8, record.page_count);
    out[10] = record.partial as u8;
    out[11] = 0;
    write_u32(out, 12, 0);
    Ok(BOOK_V2_SECTION_RECORD_BYTES)
}

pub fn decode_book_v2_section(input: &[u8]) -> Result<BookV2SectionRecord, CacheError> {
    require(input, BOOK_V2_SECTION_RECORD_BYTES)?;
    Ok(BookV2SectionRecord {
        section: read_u16(input, 0)?,
        spine: read_u16(input, 2)?,
        start_page: read_u32(input, 4)?,
        page_count: read_u16(input, 8)?,
        partial: input[10] != 0,
    })
}

fn valid_cache_v2_version(version: u16) -> bool {
    version == CACHE_V2_VERSION || version == CACHE_V2_COMPAT_VERSION
}

pub fn decode_page_header(input: &[u8]) -> Result<PageCacheHeader, CacheError> {
    require(input, PAGE_HEADER_BYTES)?;
    if read_u32(input, 0)? != CACHE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != CACHE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(PageCacheHeader {
        page_count: read_u16(input, 6)?,
        block_count: read_u16(input, 8)?,
        partial: input[10] != 0,
        text_bytes: read_u32(input, 12)?,
        viewport_width: read_u16(input, 16)?,
        viewport_height: read_u16(input, 18)?,
        font_config: read_u16(input, 20)?,
        bytes_consumed: read_u32(input, 24)?,
    })
}

pub fn encode_page(record: PageRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, PAGE_RECORD_BYTES)?;
    write_u16(out, 0, record.first_block);
    write_u16(out, 2, record.block_count);
    Ok(PAGE_RECORD_BYTES)
}

pub fn decode_page(input: &[u8]) -> Result<PageRecord, CacheError> {
    require(input, PAGE_RECORD_BYTES)?;
    Ok(PageRecord {
        first_block: read_u16(input, 0)?,
        block_count: read_u16(input, 2)?,
    })
}

pub fn encode_line(record: LineRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, LINE_RECORD_BYTES)?;
    write_u16(out, 0, record.first_word);
    write_u16(out, 2, record.word_count);
    write_i16(out, 4, record.x);
    write_i16(out, 6, record.y);
    write_u16(out, 8, record.width);
    out[10] = align_byte(record.align);
    out[11] = 0;
    Ok(LINE_RECORD_BYTES)
}

pub fn decode_line(input: &[u8]) -> Result<LineRecord, CacheError> {
    require(input, LINE_RECORD_BYTES)?;
    Ok(LineRecord {
        first_word: read_u16(input, 0)?,
        word_count: read_u16(input, 2)?,
        x: read_i16(input, 4)?,
        y: read_i16(input, 6)?,
        width: read_u16(input, 8)?,
        align: align_from_byte(input[10])?,
    })
}

pub fn encode_word(record: WordRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, WORD_RECORD_BYTES)?;
    write_u32(out, 0, record.text_offset);
    write_u16(out, 4, record.text_len);
    write_i16(out, 6, record.x);
    write_u16(out, 8, record.width);
    out[10] = style_byte(record.style);
    out[11] = 0;
    Ok(WORD_RECORD_BYTES)
}

pub fn decode_word(input: &[u8]) -> Result<WordRecord, CacheError> {
    require(input, WORD_RECORD_BYTES)?;
    Ok(WordRecord {
        text_offset: read_u32(input, 0)?,
        text_len: read_u16(input, 4)?,
        x: read_i16(input, 6)?,
        width: read_u16(input, 8)?,
        style: style_from_byte(input[10])?,
    })
}

pub fn encode_block(record: BlockRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, BLOCK_RECORD_BYTES)?;
    write_u32(out, 0, record.text_offset);
    write_u16(out, 4, record.text_len);
    out[6] = record.line_count;
    out[7] = role_byte(record.role);
    out[8] = style_byte(record.style);
    out[9] = align_byte(record.align);
    write_u16(out, 10, 0);
    Ok(BLOCK_RECORD_BYTES)
}

pub fn decode_block(input: &[u8]) -> Result<BlockRecord, CacheError> {
    require(input, BLOCK_RECORD_BYTES)?;
    Ok(BlockRecord {
        text_offset: read_u32(input, 0)?,
        text_len: read_u16(input, 4)?,
        line_count: input[6],
        role: role_from_byte(input[7])?,
        style: style_from_byte(input[8])?,
        align: align_from_byte(input[9])?,
    })
}

pub fn encode_cover_header(header: CoverCacheHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, COVER_HEADER_BYTES)?;
    out[..4].copy_from_slice(COVER_MAGIC);
    out[4] = COVER_VERSION;
    write_u16(out, 5, header.width);
    write_u16(out, 7, header.height);
    write_u16(out, 9, header.stride);
    out[11] = 0;
    Ok(COVER_HEADER_BYTES)
}

pub fn decode_cover_header(input: &[u8]) -> Result<CoverCacheHeader, CacheError> {
    require(input, COVER_HEADER_BYTES)?;
    if &input[..4] != COVER_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if input[4] != COVER_VERSION {
        return Err(CacheError::BadVersion);
    }
    if input[11] != 0 {
        return Err(CacheError::BadLength);
    }
    let header = CoverCacheHeader {
        width: read_u16(input, 5)?,
        height: read_u16(input, 7)?,
        stride: read_u16(input, 9)?,
    };
    if header != CoverCacheHeader::x4_dock_clean() {
        return Err(CacheError::BadLength);
    }
    Ok(header)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TocFileHeader {
    pub source_hash: u32,
    pub source_size: u32,
    pub chapter_count: u16,
}

#[derive(Clone, Copy)]
pub struct TocChapterRecord {
    pub spine_index: i16,
    pub level: u8,
    pub title_len: u8,
    pub title: [u8; TOC_CHAPTER_TITLE_BYTES],
}

impl TocChapterRecord {
    pub fn title_str(&self) -> &str {
        let len = (self.title_len as usize).min(TOC_CHAPTER_TITLE_BYTES);
        core::str::from_utf8(&self.title[..len]).unwrap_or("")
    }
}

/// Build a record from a title, truncating to the title budget on a UTF-8
/// char boundary so `title_str` always decodes.
pub fn toc_chapter_record(title: &str, level: u8, spine_index: i16) -> TocChapterRecord {
    let mut len = title.len().min(TOC_CHAPTER_TITLE_BYTES);
    while len > 0 && !title.is_char_boundary(len) {
        len -= 1;
    }
    let mut buf = [0u8; TOC_CHAPTER_TITLE_BYTES];
    buf[..len].copy_from_slice(&title.as_bytes()[..len]);
    TocChapterRecord {
        spine_index,
        level,
        title_len: len as u8,
        title: buf,
    }
}

pub fn encode_toc_file_header(header: TocFileHeader, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_FILE_HEADER_BYTES)?;
    write_u32(out, 0, TOC_FILE_MAGIC);
    write_u16(out, 4, TOC_FILE_VERSION);
    write_u16(out, 6, header.chapter_count);
    write_u32(out, 8, header.source_hash);
    write_u32(out, 12, header.source_size);
    Ok(TOC_FILE_HEADER_BYTES)
}

pub fn decode_toc_file_header(input: &[u8]) -> Result<TocFileHeader, CacheError> {
    require(input, TOC_FILE_HEADER_BYTES)?;
    if read_u32(input, 0)? != TOC_FILE_MAGIC {
        return Err(CacheError::BadMagic);
    }
    if read_u16(input, 4)? != TOC_FILE_VERSION {
        return Err(CacheError::BadVersion);
    }
    Ok(TocFileHeader {
        chapter_count: read_u16(input, 6)?,
        source_hash: read_u32(input, 8)?,
        source_size: read_u32(input, 12)?,
    })
}

pub fn encode_toc_chapter(record: &TocChapterRecord, out: &mut [u8]) -> Result<usize, CacheError> {
    require(out, TOC_CHAPTER_RECORD_BYTES)?;
    write_i16(out, 0, record.spine_index);
    out[2] = record.level;
    out[3] = record.title_len;
    out[4..4 + TOC_CHAPTER_TITLE_BYTES].copy_from_slice(&record.title);
    Ok(TOC_CHAPTER_RECORD_BYTES)
}

pub fn decode_toc_chapter(input: &[u8]) -> Result<TocChapterRecord, CacheError> {
    require(input, TOC_CHAPTER_RECORD_BYTES)?;
    let mut title = [0u8; TOC_CHAPTER_TITLE_BYTES];
    title.copy_from_slice(&input[4..4 + TOC_CHAPTER_TITLE_BYTES]);
    Ok(TocChapterRecord {
        spine_index: read_i16(input, 0)?,
        level: input[2],
        title_len: input[3],
        title,
    })
}

fn require(slice: &[u8], len: usize) -> Result<(), CacheError> {
    if slice.len() < len {
        Err(CacheError::BufferTooSmall)
    } else {
        Ok(())
    }
}

fn read_u16(input: &[u8], offset: usize) -> Result<u16, CacheError> {
    require(&input[offset.min(input.len())..], 2)?;
    Ok(u16::from_le_bytes([input[offset], input[offset + 1]]))
}

fn read_i16(input: &[u8], offset: usize) -> Result<i16, CacheError> {
    Ok(read_u16(input, offset)? as i16)
}

fn read_u32(input: &[u8], offset: usize) -> Result<u32, CacheError> {
    require(&input[offset.min(input.len())..], 4)?;
    Ok(u32::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
    ]))
}

fn read_u64(input: &[u8], offset: usize) -> Result<u64, CacheError> {
    require(&input[offset.min(input.len())..], 8)?;
    Ok(u64::from_le_bytes([
        input[offset],
        input[offset + 1],
        input[offset + 2],
        input[offset + 3],
        input[offset + 4],
        input[offset + 5],
        input[offset + 6],
        input[offset + 7],
    ]))
}

fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_i16(out: &mut [u8], offset: usize, value: i16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn push_hex<const N: usize>(out: &mut String<N>, value: u32, digits: u8) {
    for shift in (0..digits).rev() {
        let nibble = ((value >> (shift * 4)) & 0x0F) as u8;
        let ch = if nibble < 10 {
            b'0' + nibble
        } else {
            b'A' + nibble - 10
        };
        let _ = out.push(ch as char);
    }
}

fn push_dec3<const N: usize>(out: &mut String<N>, value: u16) {
    let value = value.min(999);
    let _ = out.push((b'0' + ((value / 100) % 10) as u8) as char);
    let _ = out.push((b'0' + ((value / 10) % 10) as u8) as char);
    let _ = out.push((b'0' + (value % 10) as u8) as char);
}

fn role_byte(role: TextRole) -> u8 {
    match role {
        TextRole::Body => 0,
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::BlockQuote => 4,
    }
}

fn role_from_byte(byte: u8) -> Result<TextRole, CacheError> {
    match byte {
        0 => Ok(TextRole::Body),
        1 => Ok(TextRole::Heading1),
        2 => Ok(TextRole::Heading2),
        3 => Ok(TextRole::Heading3),
        4 => Ok(TextRole::BlockQuote),
        _ => Err(CacheError::BadLength),
    }
}

fn style_byte(style: FontStyle) -> u8 {
    match style {
        FontStyle::Regular => 0,
        FontStyle::Italic => 1,
        FontStyle::Bold => 2,
        FontStyle::BoldItalic => 3,
    }
}

fn style_from_byte(byte: u8) -> Result<FontStyle, CacheError> {
    match byte {
        0 => Ok(FontStyle::Regular),
        1 => Ok(FontStyle::Italic),
        2 => Ok(FontStyle::Bold),
        3 => Ok(FontStyle::BoldItalic),
        _ => Err(CacheError::BadLength),
    }
}

fn align_byte(align: TextAlign) -> u8 {
    match align {
        TextAlign::Left => 0,
        TextAlign::Center => 1,
        TextAlign::Justify => 2,
    }
}

fn align_from_byte(byte: u8) -> Result<TextAlign, CacheError> {
    match byte {
        0 => Ok(TextAlign::Left),
        1 => Ok(TextAlign::Center),
        2 => Ok(TextAlign::Justify),
        _ => Err(CacheError::BadLength),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_cache_records_round_trip() {
        let header = PageCacheHeader {
            page_count: 2,
            block_count: 3,
            text_bytes: 17,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 1,
            bytes_consumed: 4096,
            partial: true,
        };
        let page = PageRecord {
            first_block: 1,
            block_count: 2,
        };
        let block = BlockRecord {
            text_offset: 42,
            text_len: 11,
            line_count: 2,
            role: TextRole::Heading2,
            style: FontStyle::BoldItalic,
            align: TextAlign::Center,
        };

        let mut bytes = [0u8; 48];
        encode_page_header(header, &mut bytes[..PAGE_HEADER_BYTES]).expect("header encodes");
        encode_page(
            page,
            &mut bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES],
        )
        .expect("page encodes");
        encode_block(
            block,
            &mut bytes[PAGE_HEADER_BYTES + PAGE_RECORD_BYTES
                ..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES + BLOCK_RECORD_BYTES],
        )
        .expect("block encodes");

        assert_eq!(
            decode_page_header(&bytes[..PAGE_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_page(&bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES]).unwrap(),
            page
        );
        assert_eq!(
            decode_block(
                &bytes[PAGE_HEADER_BYTES + PAGE_RECORD_BYTES
                    ..PAGE_HEADER_BYTES + PAGE_RECORD_BYTES + BLOCK_RECORD_BYTES],
            )
            .unwrap(),
            block
        );
    }

    #[test]
    fn book_cache_records_round_trip() {
        let header = BookCacheHeader {
            spine_count: 1,
            toc_count: 1,
            string_bytes: 27,
        };
        let spine = SpineRecord {
            href_offset: 7,
            href_len: 12,
            toc_index: -1,
            byte_size: 1234,
        };
        let toc = TocRecord {
            title_offset: 20,
            title_len: 5,
            href_offset: 7,
            href_len: 12,
            anchor_offset: 0,
            anchor_len: 0,
            level: 2,
            spine_index: -1,
        };
        let mut bytes = [0u8; BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES];
        encode_book_header(header, &mut bytes[..BOOK_HEADER_BYTES]).expect("book header encodes");
        encode_spine(
            spine,
            &mut bytes[BOOK_HEADER_BYTES..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES],
        )
        .expect("spine encodes");
        encode_toc(
            toc,
            &mut bytes[BOOK_HEADER_BYTES + SPINE_RECORD_BYTES
                ..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES],
        )
        .expect("toc encodes");

        assert_eq!(
            decode_book_header(&bytes[..BOOK_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_spine(&bytes[BOOK_HEADER_BYTES..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES])
                .unwrap(),
            spine
        );
        assert_eq!(
            decode_toc(
                &bytes[BOOK_HEADER_BYTES + SPINE_RECORD_BYTES
                    ..BOOK_HEADER_BYTES + SPINE_RECORD_BYTES + TOC_RECORD_BYTES],
            )
            .unwrap(),
            toc
        );
    }

    #[test]
    fn section_cache_records_round_trip() {
        let header = SectionHeader {
            page_count: 1,
            block_count: 1,
            line_count: 1,
            word_count: 2,
            text_bytes: 13,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 2,
            bytes_consumed: 8192,
            total_bytes: 12_000,
            partial: true,
        };
        let line = LineRecord {
            first_word: 0,
            word_count: 2,
            x: 8,
            y: 24,
            width: 760,
            align: TextAlign::Justify,
        };
        let word = WordRecord {
            text_offset: 6,
            text_len: 7,
            x: 120,
            width: 54,
            style: FontStyle::Italic,
        };
        let mut bytes = [0u8; SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES];

        encode_section_header(header, &mut bytes[..SECTION_HEADER_BYTES])
            .expect("section header encodes");
        encode_line(
            line,
            &mut bytes[SECTION_HEADER_BYTES..SECTION_HEADER_BYTES + LINE_RECORD_BYTES],
        )
        .expect("line encodes");
        encode_word(
            word,
            &mut bytes[SECTION_HEADER_BYTES + LINE_RECORD_BYTES
                ..SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES],
        )
        .expect("word encodes");

        assert_eq!(
            decode_section_header(&bytes[..SECTION_HEADER_BYTES]).unwrap(),
            header
        );
        assert_eq!(
            decode_line(&bytes[SECTION_HEADER_BYTES..SECTION_HEADER_BYTES + LINE_RECORD_BYTES])
                .unwrap(),
            line
        );
        assert_eq!(
            decode_word(
                &bytes[SECTION_HEADER_BYTES + LINE_RECORD_BYTES
                    ..SECTION_HEADER_BYTES + LINE_RECORD_BYTES + WORD_RECORD_BYTES],
            )
            .unwrap(),
            word
        );
        assert_eq!(
            section_cache_size(header),
            SECTION_HEADER_BYTES
                + PAGE_RECORD_BYTES
                + BLOCK_RECORD_BYTES
                + 1
                + LINE_RECORD_BYTES
                + WORD_RECORD_BYTES * 2
                + 13
        );
    }

    #[test]
    fn section_v2_cache_records_round_trip() {
        let header = SectionV2Header {
            source_hash: 0x1234_5678,
            source_size: 98_765,
            spine: 7,
            page_count: 2,
            block_count: 3,
            text_bytes: 19,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 2,
            custom_font_identity: 0x1122_3344_5566_7788,
            bytes_consumed: 8192,
            total_bytes: 12_000,
            partial: true,
        };
        let mut bytes = [0u8; SECTION_V2_HEADER_BYTES];
        encode_section_v2_header(header, &mut bytes).expect("section v2 header encodes");

        assert_eq!(decode_section_v2_header(&bytes).unwrap(), header);
        assert_eq!(
            section_v2_cache_size(header),
            SECTION_V2_HEADER_BYTES + PAGE_RECORD_BYTES * 2 + BLOCK_RECORD_BYTES * 3 + 3 + 19
        );

        bytes[4] = CACHE_VERSION as u8;
        bytes[5] = 0;
        assert_eq!(
            decode_section_v2_header(&bytes),
            Err(CacheError::BadVersion)
        );
    }

    #[test]
    fn book_v2_cache_records_round_trip() {
        let header = BookV2Header {
            source_hash: 0x1234_5678,
            source_size: 98_765,
            total_pages: 123,
            section_count: 2,
            spine_count: 9,
            toc_count: 4,
            toc_text_bytes: 128,
            title_text_bytes: 20,
            author_text_bytes: 18,
            viewport_width: 800,
            viewport_height: 480,
            font_config: 1,
            custom_font_identity: 0x8877_6655_4433_2211,
            partial: true,
        };
        let section = BookV2SectionRecord {
            section: 1,
            spine: 7,
            start_page: 42,
            page_count: 12,
            partial: false,
        };
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        let mut section_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];

        encode_book_v2_header(header, &mut header_bytes).expect("book v2 header encodes");
        encode_book_v2_section(section, &mut section_bytes).expect("book v2 section encodes");

        assert_eq!(decode_book_v2_header(&header_bytes).unwrap(), header);
        assert_eq!(decode_book_v2_section(&section_bytes).unwrap(), section);
        assert_eq!(
            book_v2_cache_size(header),
            BOOK_V2_HEADER_BYTES
                + BOOK_V2_SECTION_RECORD_BYTES * 2
                + TOC_RECORD_BYTES * 4
                + 128
                + 20
                + 18
        );

        header_bytes[4] = CACHE_VERSION as u8;
        header_bytes[5] = 0;
        assert_eq!(
            decode_book_v2_header(&header_bytes),
            Err(CacheError::BadVersion)
        );
    }

    #[test]
    fn artifact_names_and_cache_key_are_stable() {
        assert_eq!(CACHE_ROOT_DIR, "XTEINK");
        assert_eq!(CACHE_DIR, "CACHE");
        assert_eq!(CACHE_V2_DIR, "CACHE2");
        assert_eq!(CACHE_SECTIONS_DIR, "SECTIONS");
        assert_eq!(CACHE_BOOK_FILE, "BOOK.BIN");
        assert_eq!(CACHE_COVER_FILE, "COVER.BIN");
        assert_eq!(CACHE_STATE_FILE, "STATE.BIN");
        assert_eq!(
            cache_key_for("/books/Book.epub", 12_345).as_str(),
            "EEE2AC55"
        );

        let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
        section_file_name(7, &mut name);
        assert_eq!(name.as_str(), "S007.BIN");
        section_file_name(1234, &mut name);
        assert_eq!(name.as_str(), "S999.BIN");
    }

    #[test]
    fn cover_cache_header_round_trips_and_validates_shape() {
        let header = CoverCacheHeader::x4_dock_clean();
        let mut bytes = [0u8; COVER_HEADER_BYTES];
        encode_cover_header(header, &mut bytes).expect("cover header encodes");

        assert_eq!(decode_cover_header(&bytes).unwrap(), header);
        assert_eq!(COVER_BYTES, 7878);

        bytes[0] = b'?';
        assert_eq!(decode_cover_header(&bytes), Err(CacheError::BadMagic));
        bytes[0] = b'X';
        bytes[9] = 1;
        assert_eq!(decode_cover_header(&bytes), Err(CacheError::BadLength));
    }

    #[test]
    fn toc_chapter_records_round_trip_and_truncate() {
        let header = TocFileHeader {
            source_hash: 0xABCD_1234,
            source_size: 1_726_241,
            chapter_count: 242,
        };
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        encode_toc_file_header(header, &mut header_bytes).expect("toc header encodes");
        assert_eq!(decode_toc_file_header(&header_bytes).unwrap(), header);

        // A short title survives a round-trip intact.
        let short = toc_chapter_record("Chapter 12", 1, 45);
        let mut bytes = [0u8; TOC_CHAPTER_RECORD_BYTES];
        encode_toc_chapter(&short, &mut bytes).expect("toc record encodes");
        let back = decode_toc_chapter(&bytes).unwrap();
        assert_eq!(
            (back.spine_index, back.level, back.title_str()),
            (45, 1, "Chapter 12")
        );

        // An over-budget title truncates to a valid char-boundary prefix,
        // backing off at most one multibyte char.
        let long = "A long chapter title \u{2014} reaching past the forty-four byte title budget";
        let record = toc_chapter_record(long, 2, -1);
        assert!(record.title_str().len() <= TOC_CHAPTER_TITLE_BYTES);
        assert!(record.title_str().len() >= TOC_CHAPTER_TITLE_BYTES - 3);
        assert!(long.starts_with(record.title_str()));
    }
}
