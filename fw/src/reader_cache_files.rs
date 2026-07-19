use crate::reader_layout;
use crate::reader_store::{
    ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS, MAX_OVERVIEW_CHAPTERS,
    MAX_SD_TOC_ITEMS, MAX_SD_TOC_TEXT_BYTES,
};
use display::font::FontStyle;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::{
    decode_block, decode_book_v2_header, decode_book_v2_section, decode_cover_header, decode_page,
    decode_section_v2_header, decode_toc, decode_toc_chapter, decode_toc_file_header, encode_block,
    encode_book_v2_header, encode_book_v2_section, encode_content_header,
    encode_content_record_header, encode_page, encode_section_v2_header, encode_toc,
    encode_toc_file_header, section_file_name, BookV2Header, BookV2SectionRecord, ContentHeader,
    ContentRecordHeader, SectionV2Header, TocFileHeader, BLOCK_RECORD_BYTES, BOOK_V2_HEADER_BYTES,
    BOOK_V2_SECTION_RECORD_BYTES, CACHE_BOOK_FILE, CACHE_CONTENT_FILE, CACHE_COVER_FILE,
    CACHE_ROOT_DIR, CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES, CACHE_STATE_FILE, CACHE_TOC_FILE,
    CACHE_V2_DIR, CONTENT_HEADER_BYTES, CONTENT_RECORD_HEADER_BYTES, COVER_HEADER_BYTES,
    PAGE_RECORD_BYTES, SECTION_V2_HEADER_BYTES, TOC_CHAPTER_RECORD_BYTES, TOC_FILE_HEADER_BYTES,
    TOC_RECORD_BYTES,
};
use proto::font_pack::{
    decode_font_pack_name, FontPackFaceRecord, FontPackHeader, FONT_PACK_DIR,
    FONT_PACK_FACE_RECORD_BYTES, FONT_PACK_FILE, FONT_PACK_HEADER_BYTES,
};

use proto::durable::{
    decode_durable_record, encode_durable_record, generation_is_newer, DURABLE_MAX_BYTES,
    DURABLE_OVERHEAD,
};

/// Read and validate one durable generation file; `payload` receives the
/// record body and the valid record's generation is returned. Any missing,
/// short, oversized, or corrupt file reads as `None`.
fn read_generation_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
    magic: [u8; 4],
    payload: &mut [u8],
) -> Option<u32>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let total = payload.len().checked_add(DURABLE_OVERHEAD)?;
    if total > DURABLE_MAX_BYTES {
        return None;
    }
    let file = directory.open_file_in_dir(name, Mode::ReadOnly).ok()?;
    if file.length() as usize != total {
        return None;
    }
    let mut bytes = [0u8; DURABLE_MAX_BYTES];
    read_exact_file(&file, &mut bytes[..total]).ok()?;
    decode_durable_record(magic, &bytes[..total], payload)
}

/// Read the newest valid generation out of an A/B file pair into `payload`.
/// False means neither side holds a valid record.
fn read_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut other = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, payload);
    let b = read_generation_file(directory, names[1], magic, &mut other[..payload.len()]);
    match (a, b) {
        (None, None) => false,
        (Some(_), None) => true,
        (None, Some(_)) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(a), Some(b)) if generation_is_newer(b, a) => {
            payload.copy_from_slice(&other[..payload.len()]);
            true
        }
        (Some(_), Some(_)) => true,
    }
}

/// Write `payload` as the next generation of an A/B file pair, overwriting
/// the *older* side so the newest survivor is never the one mid-write, then
/// prove the write by re-reading it through the validating read path.
fn write_two_generation<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    directory: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    names: [&str; 2],
    magic: [u8; 4],
    payload: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut scratch = [0u8; DURABLE_MAX_BYTES];
    let a = read_generation_file(directory, names[0], magic, &mut scratch[..payload.len()]);
    let b = read_generation_file(directory, names[1], magic, &mut scratch[..payload.len()]);
    let (target, generation) = match (a, b) {
        (Some(a), Some(b)) if generation_is_newer(b, a) => (0, b.wrapping_add(1)),
        (Some(a), Some(_)) => (1, a.wrapping_add(1)),
        (Some(a), None) => (1, a.wrapping_add(1)),
        (None, Some(b)) => (0, b.wrapping_add(1)),
        (None, None) => (0, 1),
    };
    let mut record = [0u8; DURABLE_MAX_BYTES];
    let total = encode_durable_record(magic, generation, payload, &mut record)?;
    {
        let file = directory
            .open_file_in_dir(names[target], Mode::ReadWriteCreateOrTruncate)
            .map_err(|_| ())?;
        file.write(&record[..total]).map_err(|_| ())?;
    }
    let mut verify = [0u8; DURABLE_MAX_BYTES];
    let verified = read_generation_file(
        directory,
        names[target],
        magic,
        &mut verify[..payload.len()],
    );
    if verified == Some(generation) && &verify[..payload.len()] == payload {
        Ok(())
    } else {
        Err(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustomFontManifest {
    pub(crate) name: heapless::String<{ crate::reader_store::MAX_CUSTOM_FONT_NAME }>,
    pub(crate) identity: u64,
    pub(crate) faces: [FontPackFaceRecord; crate::reader_store::MAX_CUSTOM_FONT_FACES],
    pub(crate) face_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheLoadResult {
    Hit { pages: usize, repaginated: bool },
    Miss,
    Invalid,
    TooShort { pages: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BookIndexLoadResult {
    Hit,
    Miss,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoverLoadResult {
    Hit,
    Miss,
    Invalid,
}

pub(crate) fn ensure_v2_cache_dirs<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let cache = open_or_make_dir(&xteink, CACHE_V2_DIR)?;
    let book = open_or_make_dir(&cache, key)?;
    let _ = open_or_make_dir(&book, CACHE_SECTIONS_DIR)?;
    Ok(())
}

const POSITION_FILE: &str = "POS.BIN";
const POSITION_GENERATIONS: [&str; 2] = ["POSA.BIN", "POSB.BIN"];
/// MarigoldOS v0.4.x durable-position magic; keep byte-identical so cards
/// carry reading positions between the two firmwares.
const POSITION_DURABLE_MAGIC: [u8; 4] = *b"MGPS";
const POSITION_MAGIC: &[u8; 4] = b"X4PS";
const POSITION_VERSION: u8 = 1;
const POSITION_BYTES: usize = 15;

/// Panel-geometry salt mixed into the position checksum. The stored screen
/// is a page-within-chapter index under this panel's pagination, so a
/// position written on a differently sized panel (an SD card moved between
/// an X4 and an X3) is meaningless. Zero on the X4 keeps every existing
/// POS.BIN validating byte-for-byte; the X3's non-zero salt makes an
/// X4-written record fail the checksum, so the reader resumes at book start
/// rather than a stale page. The chapter would survive, but there is no
/// separate progress source to reconcile it against, so full reset is the
/// honest fallback.
const POSITION_GEOMETRY_SALT: u32 = (display::WIDTH as u32 ^ display::HEIGHT as u32) ^ (800 ^ 480);

// The salt must vanish on the X4 or an upgrade would reject every existing
// POS.BIN; guard the backward-compat guarantee at compile time.
#[cfg(not(feature = "device-x3"))]
const _: () = assert!(POSITION_GEOMETRY_SALT == 0);

fn encode_position(chapter: u16, screen: u32) -> [u8; POSITION_BYTES] {
    let mut out = [0u8; POSITION_BYTES];
    out[..4].copy_from_slice(POSITION_MAGIC);
    out[4] = POSITION_VERSION;
    out[5..7].copy_from_slice(&chapter.to_le_bytes());
    out[7..11].copy_from_slice(&screen.to_le_bytes());
    let sum = position_checksum(&out[..11]);
    out[11..15].copy_from_slice(&sum.to_le_bytes());
    out
}

/// Byte sum salted with the panel geometry. Salt is 0 on the X4 (its
/// `WIDTH ^ HEIGHT` cancels the `800 ^ 480` term) so historical checksums
/// are unchanged; any other geometry shifts every checksum.
fn position_checksum(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|byte| *byte as u32)
        .sum::<u32>()
        .wrapping_add(POSITION_GEOMETRY_SALT)
}

fn decode_position(bytes: &[u8]) -> Option<(u16, u32)> {
    if bytes.len() < POSITION_BYTES || &bytes[..4] != POSITION_MAGIC || bytes[4] != POSITION_VERSION
    {
        return None;
    }
    let sum = position_checksum(&bytes[..11]);
    if bytes[11..15] != sum.to_le_bytes() {
        return None;
    }
    let chapter = u16::from_le_bytes([bytes[5], bytes[6]]);
    let screen = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
    Some((chapter, screen))
}

/// Per-book reading position beside the book's cache records, so
/// switching books no longer abandons the previous one's place.
pub(crate) fn write_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    chapter: u16,
    screen: u32,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    let cache = open_or_make_dir(&xteink, CACHE_V2_DIR)?;
    let book = open_or_make_dir(&cache, key)?;
    write_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &encode_position(chapter, screen),
    )
}

pub(crate) fn read_position_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<(u16, u32)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book = cache.open_dir(key).ok()?;
    let mut bytes = [0u8; POSITION_BYTES];
    if read_two_generation(
        &book,
        POSITION_GENERATIONS,
        POSITION_DURABLE_MAGIC,
        &mut bytes,
    ) {
        return decode_position(&bytes);
    }
    // Legacy single-file fallback, kept readable so an upgrade resumes at
    // the pre-durable position; the next write lands on the A/B pair.
    let file = book.open_file_in_dir(POSITION_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    decode_position(&bytes[..len])
}

pub(crate) fn write_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: hal_ext::nvm::AppStateRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &xteink,
        STATE_GENERATIONS,
        STATE_DURABLE_MAGIC,
        &record.encode(),
    )
}

const STATE_GENERATIONS: [&str; 2] = ["STATEA.BIN", "STATEB.BIN"];
/// MarigoldOS v0.4.x durable-state magic; byte-identical for card interchange.
const STATE_DURABLE_MAGIC: [u8; 4] = *b"MGST";

/// Read the newest valid STATEA/STATEB generation, falling back to the
/// legacy `/XTEINK/STATE.BIN`. Returns None when every copy is absent,
/// short, or fails its checksum.
pub(crate) fn read_state_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<hal_ext::nvm::AppStateRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; hal_ext::nvm::AppStateRecord::ENCODED_LEN];
    if read_two_generation(&xteink, STATE_GENERATIONS, STATE_DURABLE_MAGIC, &mut bytes) {
        return hal_ext::nvm::AppStateRecord::decode(&bytes);
    }
    let file = xteink
        .open_file_in_dir(CACHE_STATE_FILE, Mode::ReadOnly)
        .ok()?;
    // One read suffices for a 32-byte record; shorter V1/V2 files decode
    // from their actual length.
    let len = file.read(&mut bytes).ok()?;
    hal_ext::nvm::AppStateRecord::decode(&bytes[..len])
}

pub(crate) fn read_custom_font_manifest<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<CustomFontManifest>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let fonts = xteink.open_dir(FONT_PACK_DIR).ok()?;
    let file = fonts
        .open_file_in_dir(FONT_PACK_FILE, Mode::ReadOnly)
        .ok()?;
    let mut header_bytes = [0u8; FONT_PACK_HEADER_BYTES];
    if file.read(&mut header_bytes).ok()? != FONT_PACK_HEADER_BYTES {
        return None;
    }
    let header = FontPackHeader::decode(&header_bytes).ok()?;
    if file.length() != header.total_len {
        return None;
    }
    let face_count = usize::from(header.face_count).min(crate::reader_store::MAX_CUSTOM_FONT_FACES);
    let mut faces = [FontPackFaceRecord::EMPTY; crate::reader_store::MAX_CUSTOM_FONT_FACES];
    file.seek_from_start(header.face_table_offset).ok()?;
    let mut face_bytes = [0u8; FONT_PACK_FACE_RECORD_BYTES];
    for face in faces.iter_mut().take(face_count) {
        if file.read(&mut face_bytes).ok()? != FONT_PACK_FACE_RECORD_BYTES {
            return None;
        }
        *face = FontPackFaceRecord::decode(&face_bytes).ok()?;
    }
    file.seek_from_start(header.name_offset).ok()?;
    let mut name_bytes = [0u8; proto::font_pack::FONT_PACK_MAX_NAME_BYTES];
    let name_len = header.name_len as usize;
    if file.read(&mut name_bytes[..name_len]).ok()? != name_len {
        return None;
    }
    let name = decode_font_pack_name(header, &name_bytes[..name_len]).ok()?;
    Some(CustomFontManifest {
        name,
        identity: header.identity,
        faces,
        face_count,
    })
}

const WIFI_FILE: &str = "WIFI.BIN";
const WIFI_GENERATIONS: [&str; 2] = ["WIFIA.BIN", "WIFIB.BIN"];
/// MarigoldOS v0.4.x durable-credentials magic; byte-identical for card
/// interchange.
const WIFI_DURABLE_MAGIC: [u8; 4] = *b"MGWF";

/// Write the onboarding portal's credentials to alternating WIFIA/WIFIB.
pub(crate) fn write_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record: hal_ext::nvm::WifiCredentialsRecord,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = open_or_make_dir(root, CACHE_ROOT_DIR)?;
    write_two_generation(
        &xteink,
        WIFI_GENERATIONS,
        WIFI_DURABLE_MAGIC,
        &record.encode(),
    )
}

/// Delete every stored credential copy (legacy WIFI.BIN and both
/// generations); missing files count as success.
pub(crate) fn delete_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return true;
    };
    let mut ok = true;
    for name in [WIFI_FILE, WIFI_GENERATIONS[0], WIFI_GENERATIONS[1]] {
        ok &= upload_store::remove_file_reclaiming_clusters(&xteink, name)
            != upload_store::RemoveStatus::Failed;
    }
    ok
}

/// Read the newest WIFIA/WIFIB generation, falling back to legacy WIFI.BIN;
/// None when every copy is missing, short, or corrupt.
pub(crate) fn read_wifi_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> Option<hal_ext::nvm::WifiCredentialsRecord>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let mut bytes = [0u8; hal_ext::nvm::WifiCredentialsRecord::ENCODED_LEN];
    if read_two_generation(&xteink, WIFI_GENERATIONS, WIFI_DURABLE_MAGIC, &mut bytes) {
        return hal_ext::nvm::WifiCredentialsRecord::decode(&bytes);
    }
    let file = xteink.open_file_in_dir(WIFI_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    hal_ext::nvm::WifiCredentialsRecord::decode(&bytes[..len])
}

pub(crate) fn load_v2_cover_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    library: &mut ReaderStore,
) -> CoverLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_cover_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; COVER_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return CoverLoadResult::Invalid;
        }
        let Ok(header) = decode_cover_header(&header_bytes) else {
            return CoverLoadResult::Invalid;
        };
        // Read straight into the store's cover buffer: a stack copy here is
        // an ~8 KB frame on a path that already runs near the stack floor.
        if read_exact_file(file, library.cover_bits_mut()).is_err() {
            library.clear_cover();
            return CoverLoadResult::Invalid;
        }
        library.finish_cover_load(header.width, header.height);
        CoverLoadResult::Hit
    })
    .unwrap_or(CoverLoadResult::Miss)
}

/// Read just the book's total page count from the V2 index header,
/// without loading any section records. Used at boot restore so the Home
/// progress bar has a denominator before the book is opened. Returns 0 if the
/// index is missing, stale, or for another book.
pub(crate) fn read_v2_book_total_pages<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &ReaderStore,
) -> u32
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return 0;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return 0;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.custom_font_identity != library.custom_font_identity()
        {
            return 0;
        }
        header.total_pages
    })
    .unwrap_or(0)
}

/// Bounds the TOC and label counts a BOOK.BIN header may claim before any
/// body loader trusts them.
fn v2_toc_label_bounds_ok(header: &BookV2Header) -> bool {
    header.toc_count as usize <= MAX_SD_TOC_ITEMS
        && header.toc_text_bytes as usize <= MAX_SD_TOC_TEXT_BYTES
        && header.title_text_bytes as usize <= 64
        && header.author_text_bytes as usize <= 64
}

/// Read the TOC records and text at the file's current position (just past
/// the section records) into the library. The one decoder of BOOK.BIN's
/// TOC body — `load_v2_book_index` and `load_v2_book_labels_and_toc` both
/// call it. On any failure the library's TOC is left cleared and false is
/// returned.
fn read_v2_toc_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library.clear_toc();
    if !read_records_batched(
        file,
        TOC_RECORD_BYTES,
        header.toc_count as usize,
        |index, bytes| {
            let Ok(record) = decode_toc(bytes) else {
                return false;
            };
            if !toc_record_fits_text(record, header.toc_text_bytes) {
                return false;
            }
            library.toc[index] = record;
            library.toc_page[index] = 0;
            true
        },
    ) {
        library.clear_toc();
        return false;
    }
    if header.toc_text_bytes > 0 {
        let text_len = header.toc_text_bytes as usize;
        if read_exact_file(file, &mut library.toc_text[..text_len]).is_err() {
            library.clear_toc();
            return false;
        }
        library.toc_text_len = text_len;
        library.toc_count = header.toc_count as usize;
    }
    true
}

/// Read the title/author labels at the file's current position (just past
/// the TOC text) and publish them to the library — the shared tail of both
/// BOOK.BIN body loaders. A book with neither label leaves the store's
/// labels untouched.
fn read_v2_labels_into_library<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut title = [0u8; 64];
    let mut author = [0u8; 64];
    let mut title_str = "";
    let mut author_str = "";
    if header.title_text_bytes > 0 {
        let title_len = header.title_text_bytes as usize;
        if read_exact_file(file, &mut title[..title_len]).is_err() {
            return false;
        }
        let Ok(parsed_title) = core::str::from_utf8(&title[..title_len]) else {
            return false;
        };
        title_str = parsed_title;
    }
    if header.author_text_bytes > 0 {
        let author_len = header.author_text_bytes as usize;
        if read_exact_file(file, &mut author[..author_len]).is_err() {
            return false;
        }
        let Ok(parsed_author) = core::str::from_utf8(&author[..author_len]) else {
            return false;
        };
        author_str = parsed_author;
    }
    if header.title_text_bytes > 0 || header.author_text_bytes > 0 {
        library.set_book_labels(title_str, author_str);
    }
    true
}

pub(crate) fn load_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> BookIndexLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return BookIndexLoadResult::Invalid;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return BookIndexLoadResult::Invalid;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.font_config
                != reader_layout::reader_layout_config(library.type_settings(), library.portrait())
            || header.custom_font_identity != library.custom_font_identity()
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
            || header.total_pages == 0
        {
            return BookIndexLoadResult::Invalid;
        }
        let mut sections = [EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS];
        if !read_records_batched(
            file,
            BOOK_V2_SECTION_RECORD_BYTES,
            header.section_count as usize,
            |index, bytes| {
                let Ok(record) = decode_book_v2_section(bytes) else {
                    return false;
                };
                if record.page_count == 0 {
                    return false;
                }
                sections[index] = record;
                true
            },
        ) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_toc_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        if !read_v2_labels_into_library(file, &header, library) {
            return BookIndexLoadResult::Invalid;
        }
        library.set_book_index(
            header.total_pages,
            header.partial,
            &sections[..header.section_count as usize],
        );
        BookIndexLoadResult::Hit
    })
    .unwrap_or(BookIndexLoadResult::Miss)
}

/// Read just the stored EPUB title from a book's v2 cache index, skipping the
/// section records and the rest of the body. The Library list uses this to
/// label books whose on-disk name can't carry a real title (8.3 upload names)
/// with the title learned the last time the book was opened. Returns false
/// (leaving `out` untouched) when there is no cache for the book, the cached
/// identity doesn't match, or the cache holds no title.
pub(crate) fn read_cached_book_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.title_text_bytes == 0
            || header.title_text_bytes as usize > 64
        {
            return false;
        }
        // The title text sits after the header, the section records, and the
        // TOC block (records + text) -- the same body order write_v2_book_index
        // lays down and load_v2_book_index reads through.
        let title_offset = BOOK_V2_HEADER_BYTES as u32
            + header.section_count as u32 * BOOK_V2_SECTION_RECORD_BYTES as u32
            + header.toc_count as u32 * TOC_RECORD_BYTES as u32
            + header.toc_text_bytes;
        if file.seek_from_start(title_offset).is_err() {
            return false;
        }
        let title_len = header.title_text_bytes as usize;
        let mut title = [0u8; 64];
        if read_exact_file(file, &mut title[..title_len]).is_err() {
            return false;
        }
        let Ok(title_str) = core::str::from_utf8(&title[..title_len]) else {
            return false;
        };
        out.clear();
        let _ = out.push_str(title_str);
        true
    })
    .unwrap_or(false)
}

/// Read a book cache's v2 header (for its stored source identity and section
/// count), or None when the cache has no readable BOOK.BIN. Used by the orphan
/// sweep to decide whether a cache still belongs to a book on the card.
pub(crate) fn read_cache_header<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<BookV2Header>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        read_exact_file(file, &mut header_bytes).ok()?;
        decode_book_v2_header(&header_bytes).ok()
    })
    .flatten()
}

/// Delete one book cache completely: every section file, BOOK/TOC/COVER, then
/// the emptied `SECTIONS/` and `<key>/` directories themselves. Directory
/// deletion refuses non-empty targets, so a cache whose header undercounts its
/// sections just leaves its shells for the next sweep pass. The global reading
/// position in XTEINK/STATE.BIN is never touched.
pub(crate) fn empty_cache_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    section_count: u16,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(cache) = xteink.open_dir(CACHE_V2_DIR) else {
        return;
    };
    {
        let Ok(book) = cache.open_dir(key) else {
            return;
        };
        if let Ok(sections) = book.open_dir(CACHE_SECTIONS_DIR) {
            let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
            for spine in 0..section_count {
                name.clear();
                section_file_name(spine, &mut name);
                let _ = upload_store::remove_file_reclaiming_clusters(&sections, name.as_str());
            }
        }
        // The SECTIONS handle has dropped; the empty directory can go now
        // (a directory entry has no chain to reclaim).
        let _ = book.delete_file_in_dir(CACHE_SECTIONS_DIR);
        let _ = upload_store::remove_file_reclaiming_clusters(&book, CACHE_BOOK_FILE);
        let _ = upload_store::remove_file_reclaiming_clusters(&book, CACHE_TOC_FILE);
        let _ = upload_store::remove_file_reclaiming_clusters(&book, CACHE_COVER_FILE);
        let _ = upload_store::remove_file_reclaiming_clusters(&book, CACHE_CONTENT_FILE);
    }
    // Likewise the book handle: closed by the scope above, deletable here
    // (a directory entry has no chain to reclaim).
    let _ = cache.delete_file_in_dir(key);
}

pub(crate) fn write_v2_book_index<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    total_pages: u32,
    sections: &[BookV2SectionRecord],
    library: &ReaderStore,
    partial: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if total_pages == 0 || sections.is_empty() || sections.len() > MAX_BOOK_SECTIONS {
        return false;
    }
    if ensure_v2_cache_dirs(root, key).is_err() {
        return false;
    }
    with_v2_book_file(root, key, Mode::ReadWriteCreateOrTruncate, |file| {
        let toc_count = library
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(u16::MAX as usize);
        let title_text_bytes = library.title.len().min(64) as u32;
        let author_text_bytes = library.author.len().min(64) as u32;
        let header = BookV2Header {
            source_hash: source_identity.0,
            source_size: source_identity.1,
            total_pages,
            section_count: sections.len().min(u16::MAX as usize) as u16,
            spine_count: sections
                .iter()
                .map(|section| section.spine as usize + 1)
                .max()
                .unwrap_or(0)
                .min(u16::MAX as usize) as u16,
            toc_count: toc_count as u16,
            toc_text_bytes: library
                .toc_text_len
                .min(MAX_SD_TOC_TEXT_BYTES)
                .min(u32::MAX as usize) as u32,
            title_text_bytes,
            author_text_bytes,
            viewport_width: 800,
            viewport_height: 480,
            font_config: reader_layout::reader_layout_config(
                library.type_settings(),
                library.portrait(),
            ),
            custom_font_identity: library.custom_font_identity(),
            partial,
        };
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if encode_book_v2_header(header, &mut bytes).is_err() {
            return false;
        }
        let mut stage = WriteStage::new(file);
        if stage.push(&bytes).is_err() {
            return false;
        }
        let mut record_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];
        for section in sections {
            if encode_book_v2_section(*section, &mut record_bytes).is_err()
                || stage.push(&record_bytes).is_err()
            {
                return false;
            }
        }
        let mut toc_bytes = [0u8; TOC_RECORD_BYTES];
        for record in library.toc.iter().take(toc_count).copied() {
            if encode_toc(record, &mut toc_bytes).is_err() || stage.push(&toc_bytes).is_err() {
                return false;
            }
        }
        if stage.flush().is_err() {
            return false;
        }
        if header.toc_text_bytes > 0
            && file
                .write(&library.toc_text[..header.toc_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        if header.title_text_bytes > 0
            && file
                .write(&library.title.as_bytes()[..header.title_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        if header.author_text_bytes > 0
            && file
                .write(&library.author.as_bytes()[..header.author_text_bytes as usize])
                .is_err()
        {
            return false;
        }
        true
    })
    .unwrap_or(false)
}

fn toc_record_fits_text(record: proto::cache::TocRecord, text_bytes: u32) -> bool {
    range_fits(record.title_offset, record.title_len, text_bytes)
        && range_fits(record.href_offset, record.href_len, text_bytes)
        && range_fits(record.anchor_offset, record.anchor_len, text_bytes)
}

fn range_fits(offset: u32, len: u16, text_bytes: u32) -> bool {
    offset
        .checked_add(len as u32)
        .map(|end| end <= text_bytes)
        .unwrap_or(false)
}

pub(crate) fn load_v2_section_by_global_page<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    global_page: u32,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(section) = library.section_for_global_page(global_page) else {
        return CacheLoadResult::Miss;
    };
    let result = load_v2_section_cache(
        root,
        key,
        source_identity,
        section.section,
        section.spine,
        section.page_count as usize,
        library,
    );
    if let CacheLoadResult::Hit { pages, repaginated } = result {
        library.set_current_section_range(section.start_page, pages);
        if repaginated {
            let _ = write_v2_section_cache(root, key, source_identity, section.section, library);
        }
    }
    result
}

fn open_or_make_dir<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    parent: &'a Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    name: &str,
) -> Result<Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match parent.open_dir(name) {
        Ok(dir) => Ok(dir),
        Err(_) => {
            let _ = parent.make_dir_in_dir(name);
            parent.open_dir(name).map_err(|_| ())
        }
    }
}

pub(crate) fn load_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    section: u16,
    expected_spine: u16,
    target_pages: usize,
    library: &mut ReaderStore,
) -> CacheLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_section_file(root, key, section, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; SECTION_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return CacheLoadResult::Invalid;
        }
        let Ok(header) = decode_section_v2_header(&header_bytes) else {
            return CacheLoadResult::Invalid;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.spine != expected_spine
        {
            return CacheLoadResult::Invalid;
        }
        let expected_config =
            reader_layout::reader_layout_config(library.type_settings(), library.portrait());
        if header.custom_font_identity != library.custom_font_identity() {
            return CacheLoadResult::Invalid;
        }
        // Cached blocks are pre-wrapped lines: they survive a spacing
        // change (heights re-walk below) but not a size change, which
        // alters every wrap point and needs the full EPUB rebuild.
        if header.font_config & !0b11 != expected_config & !0b11 {
            return CacheLoadResult::Invalid;
        }
        let layout_matches = header.font_config == expected_config;
        if !load_v2_section_body(file, header, library) {
            return CacheLoadResult::Invalid;
        }
        if !layout_matches {
            reader_layout::rebuild_page_index(library);
        }
        let pages = library.page_count;
        if pages < target_pages {
            CacheLoadResult::TooShort { pages }
        } else {
            CacheLoadResult::Hit {
                pages,
                repaginated: !layout_matches,
            }
        }
    })
    .unwrap_or(CacheLoadResult::Miss)
}

pub(crate) fn write_v2_section_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, key).is_err() {
        esp_println::println!("cache: v2 ensure dirs failed key={}", key);
        return false;
    }
    with_v2_section_file(
        root,
        key,
        section,
        Mode::ReadWriteCreateOrTruncate,
        |file| write_v2_section_body(file, source_identity, library.cached_spine, library),
    )
    .unwrap_or_else(|| {
        esp_println::println!(
            "cache: v2 open section failed key={} section={}",
            key,
            section
        );
        false
    })
}

/// Open the book's SECTIONS directory once and run `f` with it, so a whole
/// build writes tens of section files without re-walking the four-level
/// cache chain per section. Directory creation failure passes `None`: the
/// build still runs, every section write reports failure, and the book is
/// marked partial — the same degraded path as before.
pub(crate) fn with_v2_sections_dir<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    f: impl for<'a> FnOnce(Option<&Directory<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>) -> R,
) -> R
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // One handle walks the chain via change_dir, so the whole build holds a
    // single directory slot instead of the four-level ladder. The caller is
    // responsible for `ensure_v2_cache_dirs` when the tree might not exist
    // yet (the full build runs it once up front); a missing tree lands in
    // the `f(None)` fallback like any other open failure.
    let Some(mut dir) = open_v2_book_dir(root, key) else {
        return f(None);
    };
    if dir.change_dir(CACHE_SECTIONS_DIR).is_err() {
        return f(None);
    }
    f(Some(&dir))
}

/// Write one section file into an already-open SECTIONS directory — the
/// per-section body of `write_v2_section_cache` without the per-call
/// directory walk.
pub(crate) fn write_v2_section_cache_in<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sections: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    section: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(section, &mut name);
    match sections.open_file_in_dir(name.as_str(), Mode::ReadWriteCreateOrTruncate) {
        Ok(file) => write_v2_section_body(&file, source_identity, library.cached_spine, library),
        Err(_) => {
            esp_println::println!("cache: v2 open section failed section={}", section);
            false
        }
    }
}

/// Open the book's cache directory (`XTEINK/CACHE2/<key>`) with one handle
/// walked via `change_dir` — the single owner of that path walk. Opening a
/// directory another walk also passes through is fine: this embedded-sdmmc
/// rev allows duplicate directory opens (directories hold no cached
/// state); only deleting an open directory errors.
pub(crate) fn open_v2_book_dir<
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &'v Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> Option<Directory<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut dir = root.open_dir(CACHE_ROOT_DIR).ok()?;
    dir.change_dir(CACHE_V2_DIR).ok()?;
    dir.change_dir(key).ok()?;
    Some(dir)
}

/// Open the book's `CONT.BIN` (settings-independent content cache) and run
/// `f` with it.
pub(crate) fn with_v2_content_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let dir = open_v2_book_dir(root, key)?;
    let file = dir.open_file_in_dir(CACHE_CONTENT_FILE, mode).ok()?;
    Some(f(&file))
}

/// Delete the book's `CONT.BIN`. Failures are ignored — a stale or corrupt
/// content cache is only ever an accelerator, and the next full build
/// recreates it.
pub(crate) fn delete_v2_content_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(dir) = open_v2_book_dir(root, key) else {
        return;
    };
    let _ = upload_store::remove_file_reclaiming_clusters(&dir, CACHE_CONTENT_FILE);
}

/// Captures the build's `push_block` stream into `<key>/CONT.BIN` so a later
/// type-settings change replays it instead of re-reading and re-parsing the
/// EPUB. Failure is one-way and silent: the capture disables itself, and
/// `finish` deletes the partial file — CONT.BIN is purely an accelerator, so
/// the build itself never fails on its account.
pub(crate) struct ContentCapture<
    'd,
    's,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// `None` once disabled (setup or write failure).
    file: Option<File<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    stage: &'s mut [u8],
    len: usize,
    source_identity: (u32, u32),
    spine_count: u16,
}

impl<'d, 's, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    ContentCapture<'d, 's, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Create `CONT.BIN` in the book's cache dir (from `open_v2_content_dir`)
    /// and write its header with `complete = false`; the flag flips in
    /// `finish` only after the whole spine walk captured. Any failure — or
    /// `None` for the dir — returns a disabled capture.
    pub(crate) fn begin(
        dir: Option<&'d Directory<'d, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        stage: &'s mut [u8],
    ) -> Self {
        let mut capture = Self {
            file: None,
            stage,
            len: 0,
            source_identity,
            spine_count: 0,
        };
        let Some(dir) = dir else {
            return capture;
        };
        let Ok(file) = dir.open_file_in_dir(CACHE_CONTENT_FILE, Mode::ReadWriteCreateOrTruncate)
        else {
            return capture;
        };
        let mut header = [0u8; CONTENT_HEADER_BYTES];
        let encoded = encode_content_header(
            ContentHeader {
                source_hash: source_identity.0,
                source_size: source_identity.1,
                complete: false,
                spine_count: 0,
                content_len: 0,
            },
            &mut header,
        );
        if encoded.is_ok() && file.write(&header).is_ok() {
            capture.file = Some(file);
        }
        capture
    }

    /// Record one `push_block` call. The text follows the fixed record
    /// header; see `proto::cache::ContentRecordHeader`.
    pub(crate) fn push_block_record(
        &mut self,
        spine_index: u16,
        text: &str,
        role: proto::text::TextRole,
        style: proto::text::FontStyle,
        align: proto::text::TextAlign,
        paragraph_end: bool,
    ) {
        if self.file.is_none() {
            return;
        }
        let Ok(text_len) = u16::try_from(text.len()) else {
            self.file = None;
            return;
        };
        if text.len() > crate::reader_cache::READER_XHTML_SCRATCH {
            self.file = None;
            return;
        }
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len,
                role,
                style,
                align,
                paragraph_end,
                spine_end: false,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }

        self.stage_push(&header);
        self.stage_push(text.as_bytes());
    }

    /// Record the end of one spine item, so replay knows where to finish
    /// the current section run.
    pub(crate) fn spine_end(&mut self, spine_index: u16) {
        if self.file.is_none() {
            return;
        }
        self.spine_count = self.spine_count.saturating_add(1);
        let mut header = [0u8; CONTENT_RECORD_HEADER_BYTES];
        if encode_content_record_header(
            ContentRecordHeader {
                spine_index,
                text_len: 0,
                role: proto::text::TextRole::Body,
                style: proto::text::FontStyle::Regular,
                align: proto::text::TextAlign::Left,
                paragraph_end: false,
                spine_end: true,
            },
            &mut header,
        )
        .is_err()
        {
            self.file = None;
            return;
        }
        self.stage_push(&header);
    }

    /// Batch small record writes through the caller-owned staging buffer
    /// via the shared `staged_write`; any write failure disables the
    /// capture.
    fn stage_push(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        if staged_write(file, self.stage, &mut self.len, bytes).is_err() {
            self.file = None;
        }
    }

    /// Flush and mark the capture complete (`keep = true`, the whole spine
    /// walk captured cleanly), or delete the partial file through the same
    /// directory handle the capture was opened from. Returns whether a
    /// complete CONT.BIN was kept.
    pub(crate) fn finish(
        mut self,
        dir: Option<&Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        keep: bool,
    ) -> bool {
        let mut kept = false;
        if keep {
            if let Some(file) = self.file.as_ref() {
                if staged_flush(file, self.stage, &mut self.len).is_err() {
                    self.file = None;
                }
            }
            if let Some(file) = self.file.as_ref() {
                let file_len = file.length();
                let mut header = [0u8; CONTENT_HEADER_BYTES];
                kept = encode_content_header(
                    ContentHeader {
                        source_hash: self.source_identity.0,
                        source_size: self.source_identity.1,
                        complete: true,
                        spine_count: self.spine_count,
                        content_len: file_len,
                    },
                    &mut header,
                )
                .is_ok()
                    && file.seek_from_start(0).is_ok()
                    && file.write(&header).is_ok();
            }
        }
        drop(self.file.take());
        if !kept {
            if let Some(dir) = dir {
                let _ = upload_store::remove_file_reclaiming_clusters(dir, CACHE_CONTENT_FILE);
            }
        }
        kept
    }
}

/// Load only the labels (title/author) and the resident TOC copy from a
/// book's v2 index, accepting any layout config or custom-font identity:
/// the content replay path runs precisely when the index is layout-invalid,
/// but its TOC and labels are settings-independent and must survive into
/// the rewritten index. Deliberately does not touch the section index.
pub(crate) fn load_v2_book_labels_and_toc<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_book_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || !v2_toc_label_bounds_ok(&header)
        {
            return false;
        }
        let toc_offset =
            BOOK_V2_HEADER_BYTES + header.section_count as usize * BOOK_V2_SECTION_RECORD_BYTES;
        if file.seek_from_start(toc_offset as u32).is_err() {
            return false;
        }
        read_v2_toc_into_library(file, &header, library)
            && read_v2_labels_into_library(file, &header, library)
    })
    .unwrap_or(false)
}

fn with_v2_section_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    spine: u16,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let sections = book_dir.open_dir(CACHE_SECTIONS_DIR).ok()?;
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(spine, &mut name);
    let file = sections.open_file_in_dir(name.as_str(), mode).ok()?;
    Some(f(&file))
}

fn with_v2_book_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let file = book_dir.open_file_in_dir(CACHE_BOOK_FILE, mode).ok()?;
    Some(f(&file))
}

fn with_v2_toc_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let file = book_dir.open_file_in_dir(CACHE_TOC_FILE, mode).ok()?;
    Some(f(&file))
}

/// Load the on-disk chapter list (TOC.BIN) into the store's text buffer for
/// the Chapters overview. Reuses the section text buffer -- the reading
/// section is reloaded on exit -- so no resident RAM is spent on the list.
pub(crate) fn load_v2_toc_into_text<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
    window_start: usize,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            esp_println::println!("toc window: header read failed");
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            esp_println::println!("toc window: header decode failed");
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            esp_println::println!("toc window: identity mismatch");
            return false;
        }
        let total = header.chapter_count as usize;
        let start = window_start.min(total.saturating_sub(1));
        let len = (total - start).min(crate::reader_store::TOC_WINDOW_CAPACITY);
        let offset = TOC_FILE_HEADER_BYTES + start * TOC_CHAPTER_RECORD_BYTES;
        if file.seek_from_start(offset as u32).is_err() {
            esp_println::println!("toc window: seek failed");
            return false;
        }
        let bytes = len.saturating_mul(TOC_CHAPTER_RECORD_BYTES);
        let Some(buf) = library.cached_text_mut(bytes) else {
            return false;
        };
        if read_exact_file(file, buf).is_err() {
            esp_println::println!("toc window: body read failed");
            return false;
        }
        library.set_toc_window(start, len, total);
        true
    })
    .unwrap_or(false)
}

/// Fill the resident `chapter_page` map (chapter -> global start page) from
/// TOC.BIN, so the firmware can resolve the current chapter for any reading
/// page across the whole book -- past the 128-entry resident/event caps. The
/// book index must already be loaded so `page_for_spine` resolves.
pub(crate) fn load_v2_toc_page_map<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0 || header.source_size != source_identity.1 {
            return false;
        }
        let count = (header.chapter_count as usize).min(MAX_OVERVIEW_CHAPTERS);
        if !read_records_batched(
            file,
            TOC_CHAPTER_RECORD_BYTES,
            header.chapter_count as usize,
            |index, bytes| {
                if index >= MAX_OVERVIEW_CHAPTERS {
                    // Drain the rest of the file but keep only the first
                    // MAX_OVERVIEW_CHAPTERS starts.
                    return true;
                }
                let spine = i16::from_le_bytes([bytes[0], bytes[1]]);
                let page = if spine < 0 {
                    0
                } else {
                    library.page_for_spine(spine as u16).min(u16::MAX as u32) as u16
                };
                library.chapter_page[index] = page;
                true
            },
        ) {
            return false;
        }
        library.chapter_page_count = count;
        true
    })
    .unwrap_or(false)
}

/// Read one chapter's title straight from its TOC.BIN record (a single seek
/// and 48-byte read) into the resident current-chapter slot, so the Home and
/// sleep colophons can name a chapter the 128-entry resident list omits.
pub(crate) fn read_v2_toc_chapter_title<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    chapter: u16,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    with_v2_toc_file(root, key, Mode::ReadOnly, |file| {
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return false;
        }
        let Ok(header) = decode_toc_file_header(&header_bytes) else {
            return false;
        };
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || chapter >= header.chapter_count
        {
            return false;
        }
        let offset = (TOC_FILE_HEADER_BYTES + chapter as usize * TOC_CHAPTER_RECORD_BYTES) as u32;
        if file.seek_from_start(offset).is_err() {
            return false;
        }
        let mut record = [0u8; TOC_CHAPTER_RECORD_BYTES];
        if read_exact_file(file, &mut record).is_err() {
            return false;
        }
        let Ok(parsed) = decode_toc_chapter(&record) else {
            return false;
        };
        library.set_current_chapter(chapter, parsed.title_str(), source_identity);
        true
    })
    .unwrap_or(false)
}

/// Write the full chapter list to TOC.BIN: a header plus `chapter_count`
/// pre-encoded `TOC_CHAPTER_RECORD_BYTES` records (the caller assembles them
/// in a scratch buffer during the TOC parse). Keeping the list on the card
/// lets a long book's TOC stay out of the tight reader RAM.
pub(crate) fn write_v2_toc_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    chapter_count: usize,
    records: &[u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if ensure_v2_cache_dirs(root, key).is_err() {
        return false;
    }
    with_v2_toc_file(root, key, Mode::ReadWriteCreateOrTruncate, |file| {
        let header = TocFileHeader {
            source_hash: source_identity.0,
            source_size: source_identity.1,
            chapter_count: chapter_count.min(u16::MAX as usize) as u16,
        };
        let mut header_bytes = [0u8; TOC_FILE_HEADER_BYTES];
        if encode_toc_file_header(header, &mut header_bytes).is_err()
            || file.write(&header_bytes).is_err()
        {
            return false;
        }
        !records.is_empty() && file.write(records).is_ok() || records.is_empty()
    })
    .unwrap_or(false)
}

fn with_v2_cover_file<
    R,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    mode: Mode,
    f: impl for<'a> FnOnce(&File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let file = book_dir.open_file_in_dir(CACHE_COVER_FILE, mode).ok()?;
    Some(f(&file))
}

fn load_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    header: SectionV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let page_count = header.page_count as usize;
    let block_count = header.block_count as usize;
    let text_bytes = header.text_bytes as usize;
    if !library.can_hold_section(page_count, block_count, text_bytes) {
        return false;
    }
    library.clear_lines();
    if !read_records_batched(file, PAGE_RECORD_BYTES, page_count, |index, bytes| {
        let Ok(page) = decode_page(bytes) else {
            return false;
        };
        library.set_cached_page(index, page, header.spine)
    }) {
        return false;
    }
    if !read_records_batched(file, BLOCK_RECORD_BYTES, block_count, |index, bytes| {
        let Ok(block) = decode_block(bytes) else {
            return false;
        };
        library.set_cached_block(
            index,
            block,
            display_style_for_proto_style(block.style),
            header.spine,
        )
    }) {
        return false;
    }
    if !read_records_batched(file, 1, block_count, |index, bytes| {
        library.set_cached_paragraph_end(index, bytes[0] & 0b01 != 0)
            && library.set_cached_paragraph_start(index, bytes[0] & 0b10 != 0)
    }) {
        return false;
    }
    let Some(text) = library.cached_text_mut(text_bytes) else {
        return false;
    };
    if read_exact_file(file, text).is_err() {
        return false;
    }
    library.finish_cached_section(
        header.spine,
        page_count,
        block_count,
        text_bytes,
        header.partial,
    );
    true
}

fn write_v2_section_body<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    spine: u16,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let header = SectionV2Header {
        source_hash: source_identity.0,
        source_size: source_identity.1,
        spine,
        page_count: library.page_count.min(u16::MAX as usize) as u16,
        block_count: library.block_count.min(u16::MAX as usize) as u16,
        text_bytes: library.text_len.min(u32::MAX as usize) as u32,
        viewport_width: 800,
        viewport_height: 480,
        font_config: reader_layout::reader_layout_config(
            library.type_settings(),
            library.portrait(),
        ),
        custom_font_identity: library.custom_font_identity(),
        bytes_consumed: 0,
        total_bytes: 0,
        partial: library.section_partial,
    };
    let mut bytes = [0u8; SECTION_V2_HEADER_BYTES];
    if encode_section_v2_header(header, &mut bytes).is_err() || file.write(&bytes).is_err() {
        esp_println::println!("cache: v2 write header failed");
        return false;
    }
    write_section_records(file, library)
}

fn write_section_records<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut record = [0u8; 16];
    let mut stage = WriteStage::new(file);
    for page in library.pages.iter().take(library.page_count) {
        if encode_page(*page, &mut record[..PAGE_RECORD_BYTES]).is_err()
            || stage.push(&record[..PAGE_RECORD_BYTES]).is_err()
        {
            esp_println::println!("cache: write page record failed");
            return false;
        }
    }
    for block in library.blocks.iter().take(library.block_count) {
        if encode_block(*block, &mut record[..BLOCK_RECORD_BYTES]).is_err()
            || stage.push(&record[..BLOCK_RECORD_BYTES]).is_err()
        {
            esp_println::println!("cache: write block record failed");
            return false;
        }
    }
    // One flag byte per block: bit 0 marks a paragraph end, bit 1 a
    // paragraph start (the indented opening line).
    for index in 0..library.block_count {
        let end = library.block_paragraph_end[index];
        let start = library.block_paragraph_start[index];
        let flag = (end as u8) | ((start as u8) << 1);
        if stage.push(&[flag]).is_err() {
            esp_println::println!("cache: write paragraph flag failed");
            return false;
        }
    }
    if stage.flush().is_err() {
        esp_println::println!("cache: write staged records failed");
        return false;
    }
    if file.write(&library.text[..library.text_len]).is_err() {
        esp_println::println!("cache: write text failed");
        return false;
    }
    true
}

/// Staging size for batched record reads. Kept small: this sits on the
/// stack inside the EPUB open path, in the same tight budget region.
const RECORD_STAGE_BYTES: usize = 256;

/// Append `bytes` to `file` through a caller-owned staging buffer: flush
/// the stage when the bytes don't fit the remaining capacity, and bypass
/// it entirely for writes at least as large as the whole buffer. The one
/// implementation of the batching arithmetic — `WriteStage` and
/// `ContentCapture` both delegate here.
fn staged_write<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &mut [u8],
    len: &mut usize,
    bytes: &[u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if bytes.len() > buf.len() - *len {
        staged_flush(file, buf, len)?;
    }
    if bytes.len() >= buf.len() {
        return file.write(bytes).map_err(|_| ());
    }
    buf[*len..*len + bytes.len()].copy_from_slice(bytes);
    *len += bytes.len();
    Ok(())
}

/// Write out whatever `staged_write` has accumulated. `len` resets even on
/// failure so a disabled writer can't replay stale bytes.
fn staged_flush<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: &[u8],
    len: &mut usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if *len == 0 {
        return Ok(());
    }
    let result = file.write(&buf[..*len]).map_err(|_| ());
    *len = 0;
    result
}

/// Batch small writes through one staging buffer — the write-side twin of
/// `read_records_batched`. The FAT layer pays the same per-call overhead on
/// writes (block lookup plus a read-modify-write of the current sector), so
/// 1-16 byte record writes dominate section write time without it.
struct WriteStage<
    'f,
    'v,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    buf: [u8; RECORD_STAGE_BYTES],
    len: usize,
}

impl<'f, 'v, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    WriteStage<'f, 'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn new(file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Self {
        Self {
            file,
            buf: [0u8; RECORD_STAGE_BYTES],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        staged_write(self.file, &mut self.buf, &mut self.len, bytes)
    }

    fn flush(&mut self) -> Result<(), ()> {
        staged_flush(self.file, &self.buf, &mut self.len)
    }
}

/// Read `count` fixed-size records through one staging buffer instead of
/// one embedded-sdmmc read call per record; the FAT layer pays per-call
/// overhead, so 4-16 byte reads dominate section and index load time.
fn read_records_batched<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    record_len: usize,
    count: usize,
    mut apply: impl FnMut(usize, &[u8]) -> bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if record_len == 0 || record_len > RECORD_STAGE_BYTES {
        return false;
    }
    let mut stage = [0u8; RECORD_STAGE_BYTES];
    let per_batch = (RECORD_STAGE_BYTES / record_len) * record_len;
    let mut index = 0usize;
    while index < count {
        let take = ((count - index) * record_len).min(per_batch);
        if read_exact_file(file, &mut stage[..take]).is_err() {
            return false;
        }
        for chunk in stage[..take].chunks_exact(record_len) {
            if !apply(index, chunk) {
                return false;
            }
            index += 1;
        }
    }
    true
}

pub(crate) fn read_exact_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut out: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    while !out.is_empty() {
        let read = file.read(out).map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        let tmp = out;
        out = &mut tmp[read..];
    }
    Ok(())
}

fn display_style_for_proto_style(style: proto::text::FontStyle) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => FontStyle::Regular,
    }
}
