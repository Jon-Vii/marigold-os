#[path = "reader_cache_files/book_generation.rs"]
mod book_generation;
use crate::reader_layout;
use crate::reader_store::{
    page_fraction, PageAnchor, ReaderStore, SectionTable, EMPTY_TOC_RECORD, MAX_BOOK_SECTIONS,
    MAX_OVERVIEW_CHAPTERS, MAX_SD_TOC_ITEMS, MAX_SD_TOC_TEXT_BYTES, MAX_SEED_SECTIONS,
    PROVISIONAL_PAGE_BASE, PROVISIONAL_PAGE_FLOOR, SEED_SECTION_BASE,
};
use display::font::FontStyle;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::{
    book_v2_predates_images, decode_block, decode_book_v2_header, decode_book_v2_section,
    decode_cover_header, decode_page, decode_section_v2_header, decode_toc, decode_toc_chapter,
    decode_toc_file_header, encode_block, encode_book_v2_header, encode_book_v2_section,
    encode_page, encode_section_v2_header, encode_toc, encode_toc_file_header, section_file_name,
    BookV2Header, BookV2SectionRecord, SectionV2Header, TocFileHeader, BLOCK_RECORD_BYTES,
    BOOK_V2_HEADER_BYTES, BOOK_V2_SECTION_RECORD_BYTES, CACHE_BOOK_FILE, CACHE_COVER_FILE,
    CACHE_ROOT_DIR, CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES, CACHE_STATE_FILE, CACHE_TOC_FILE,
    CACHE_V2_DIR, COVER_HEADER_BYTES, PAGE_RECORD_BYTES, SECTION_V2_HEADER_BYTES,
    TOC_CHAPTER_RECORD_BYTES, TOC_FILE_HEADER_BYTES, TOC_RECORD_BYTES,
};
use proto::font_pack::{
    decode_font_pack_name, FontPackFaceRecord, FontPackHeader, FONT_PACK_DIR,
    FONT_PACK_FACE_RECORD_BYTES, FONT_PACK_FILE, FONT_PACK_HEADER_BYTES,
};

const DURABLE_VERSION: u8 = 1;
const DURABLE_OVERHEAD: usize = 13;
const DURABLE_MAX_BYTES: usize = 128;

fn durable_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    })
}

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
    if bytes[..4] != magic || bytes[4] != DURABLE_VERSION {
        return None;
    }
    let stored = u32::from_le_bytes(bytes[total - 4..total].try_into().ok()?);
    if durable_checksum(&bytes[..total - 4]) != stored {
        return None;
    }
    payload.copy_from_slice(&bytes[9..total - 4]);
    Some(u32::from_le_bytes(bytes[5..9].try_into().ok()?))
}

fn generation_is_newer(candidate: u32, current: u32) -> bool {
    let distance = candidate.wrapping_sub(current);
    distance != 0 && distance < 0x8000_0000
}

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
    let total = payload.len().checked_add(DURABLE_OVERHEAD).ok_or(())?;
    if total > DURABLE_MAX_BYTES {
        return Err(());
    }
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
    record[..4].copy_from_slice(&magic);
    record[4] = DURABLE_VERSION;
    record[5..9].copy_from_slice(&generation.to_le_bytes());
    record[9..9 + payload.len()].copy_from_slice(payload);
    let checksum = durable_checksum(&record[..total - 4]);
    record[total - 4..total].copy_from_slice(&checksum.to_le_bytes());
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
    SpacingMismatch,
    /// A valid index written before sections held image blocks (cache
    /// v27), loaded like a hit (or, with `spacing_changed`, a spacing
    /// mismatch). Its book's images, if it has any, were laid out as alt
    /// text: see `settle_index_before_images`.
    BeforeImages {
        spacing_changed: bool,
    },
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
const _: () = assert!(display::WIDTH != 800 || POSITION_GEOMETRY_SALT == 0);

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
    let file = book.open_file_in_dir(POSITION_FILE, Mode::ReadOnly).ok()?;
    let len = file.read(&mut bytes).ok()?;
    decode_position(&bytes[..len])
}

const STATE_GENERATIONS: [&str; 2] = ["STATEA.BIN", "STATEB.BIN"];
const STATE_DURABLE_MAGIC: [u8; 4] = *b"MGST";

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

/// Read the newest valid STATEA/STATEB generation, falling back to the legacy
/// `/XTEINK/STATE.BIN`. Returns None when every copy is absent or corrupt.
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

/// Delete /XTEINK/WIFI.BIN; missing file counts as success.
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
        ok &= match xteink.open_file_in_dir(name, Mode::ReadWriteTruncate) {
            Ok(file) => {
                drop(file);
                xteink.delete_file_in_dir(name).is_ok()
            }
            Err(embedded_sdmmc::Error::NotFound) => true,
            Err(_) => false,
        };
    }
    ok
}

/// Read the newest WIFIA/WIFIB generation, falling back to legacy WIFI.BIN.
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
        if header.next_spine.is_some()
            || header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.custom_font_identity != library.custom_font_identity()
        {
            return 0;
        }
        header.total_pages
    })
    .unwrap_or(0)
}

/// Pins `page` in the book's on-card pagination when that pagination was
/// laid out under another layout than the store's current one (the type
/// settings or orientation changed since it was built), so a rebuild can
/// land on the same text. The place is None when the index already matches
/// the current layout and page numbers still hold, or when the index is
/// missing, unreadable, for another source, or short of the page. Reads
/// only the header and the section records.
#[inline(never)]
pub(crate) fn read_v2_page_anchor<
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
    page: u32,
) -> PageAnchor
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let place = with_verified_v2_book_file(root, key, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        read_exact_file(file, &mut header_bytes).ok()?;
        let header = decode_book_v2_header(&header_bytes).ok()?;
        let expected_config =
            reader_layout::reader_layout_config(library.type_settings(), library.portrait());
        // A page numbered in the seed run, or in the stretch just before it.
        let provisional = header.seed_first_spine.is_some() && page >= PROVISIONAL_PAGE_FLOOR;
        if header.font_config == expected_config
            || header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || (!provisional && page >= header.total_pages)
        {
            return None;
        }
        let main = scan_page_place(
            file,
            header.section_count as usize,
            (!provisional).then_some(page),
        );
        if !provisional {
            return main;
        }
        // The seed's records follow the prefix's, just read past. A page
        // before the run pins the run's start.
        let place = scan_page_place(
            file,
            header.seed_section_count as usize,
            Some(page.max(PROVISIONAL_PAGE_BASE)),
        )?;
        Some(if page < PROVISIONAL_PAGE_BASE {
            (place.0, 0)
        } else {
            place
        })
    })
    .flatten();
    PageAnchor {
        source: source_identity,
        page,
        place,
    }
}

/// Reads the next `count` section records for `page`'s spine and how far
/// through that spine's pages it sits; without a page, just reads past them.
fn scan_page_place<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    count: usize,
    page: Option<u32>,
) -> Option<(u16, u16)>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Records run in reading order with each spine's sections together, so
    // the page's spine is whole once the next spine begins.
    let (mut spine, mut start, mut pages) = (0u16, 0u32, 0u32);
    let (mut started, mut found, mut corrupt) = (false, false, false);
    read_records_batched(file, BOOK_V2_SECTION_RECORD_BYTES, count, |_, bytes| {
        let Ok(record) = decode_book_v2_section(bytes) else {
            corrupt = true;
            return false;
        };
        if started && record.spine == spine {
            pages += u32::from(record.page_count);
        } else if found {
            return false;
        } else {
            (spine, start, pages) = (
                record.spine,
                record.start_page,
                u32::from(record.page_count),
            );
            started = true;
        }
        found |= page.is_some_and(|page| {
            page >= record.start_page && page - record.start_page < u32::from(record.page_count)
        });
        true
    });
    let page = page?;
    (found && !corrupt).then(|| (spine, page_fraction(page - start, pages)))
}

/// A book index failed validation after its sections were loaded: drop
/// them so nothing reads a half-checked table.
fn invalid_index(library: &mut ReaderStore) -> BookIndexLoadResult {
    library.book_sections.clear();
    library.seed.clear();
    BookIndexLoadResult::Invalid
}

/// Reads the seed run's records, which follow the prefix's, into the
/// store: contiguous ids from a seed base and pages from
/// `PROVISIONAL_PAGE_BASE`, spines in order within the run.
fn read_seed_records<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    header: &BookV2Header,
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library.seed.clear();
    let Some(first) = header.seed_first_spine else {
        return true;
    };
    let seed = &mut library.seed.sections;
    let mut previous_spine = first;
    let ok = read_records_batched(
        file,
        BOOK_V2_SECTION_RECORD_BYTES,
        header.seed_section_count as usize,
        |index, bytes| {
            let Ok(record) = decode_book_v2_section(bytes) else {
                return false;
            };
            if index == 0
                && (record.section < SEED_SECTION_BASE
                    || usize::from(record.section - SEED_SECTION_BASE) % MAX_SEED_SECTIONS != 0
                    || record.start_page != PROVISIONAL_PAGE_BASE)
            {
                return false;
            }
            if record.page_count == 0
                || record.spine < previous_spine
                || record
                    .start_page
                    .checked_add(u32::from(record.page_count))
                    .is_none_or(|end| end > u32::from(u16::MAX))
            {
                return false;
            }
            previous_spine = record.spine;
            seed.push(record)
        },
    );
    if !ok {
        library.seed.clear();
        return false;
    }
    library.seed.first_spine = Some(first);
    library.seed.capped = header.seed_capped;
    // The run goes on past its last laid-out spine; any it laid out empty
    // after that are laid out again, to the same nothing.
    library.seed.next_spine = (!header.seed_reaches_end).then(|| {
        library
            .seed
            .sections
            .last()
            .map_or(first, |section| section.spine.saturating_add(1))
    });
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
    with_verified_v2_book_file(root, key, |file| {
        let mut header_bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if read_exact_file(file, &mut header_bytes).is_err() {
            return BookIndexLoadResult::Invalid;
        }
        let Ok(header) = decode_book_v2_header(&header_bytes) else {
            return BookIndexLoadResult::Invalid;
        };
        let expected_config =
            reader_layout::reader_layout_config(library.type_settings(), library.portrait());
        let spacing_changed = header.font_config != expected_config;
        if header.source_hash != source_identity.0
            || header.source_size != source_identity.1
            || header.font_config & !0b11 != expected_config & !0b11
            || (spacing_changed && header.next_spine.is_some())
            || header.custom_font_identity != library.custom_font_identity()
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || header.toc_count as usize > MAX_SD_TOC_ITEMS
            || header.toc_text_bytes as usize > MAX_SD_TOC_TEXT_BYTES
            || header.title_text_bytes as usize > 64
            || header.author_text_bytes as usize > 64
            // Pages come from the prefix's sections; a seed run alone may
            // hold all there is so far.
            || (header.section_count == 0) != (header.total_pages == 0)
            || (header.total_pages == 0 && header.seed_section_count == 0)
            || header.seed_section_count as usize > MAX_SEED_SECTIONS
            || (header.seed_first_spine.is_none() && header.seed_section_count != 0)
        {
            return BookIndexLoadResult::Invalid;
        }
        // Straight into the store's table: only a load the caller has just
        // cleared the store for reaches here, and a failure clears it again.
        // (A staging copy on the stack would be 12 bytes a section.)
        let sections = &mut library.book_sections;
        sections.clear();
        let mut previous: Option<BookV2SectionRecord> = None;
        if !read_records_batched(
            file,
            BOOK_V2_SECTION_RECORD_BYTES,
            header.section_count as usize,
            |index, bytes| {
                let Ok(record) = decode_book_v2_section(bytes) else {
                    return false;
                };
                let expected_start = match previous {
                    None => 0,
                    Some(previous) => {
                        if record.spine < previous.spine
                            || usize::from(record.section) / MAX_BOOK_SECTIONS
                                != usize::from(previous.section) / MAX_BOOK_SECTIONS
                            || record.section != previous.section + 1
                        {
                            return false;
                        }
                        previous.start_page + u32::from(previous.page_count)
                    }
                };
                if record.page_count == 0
                    || usize::from(record.section) >= 2 * MAX_BOOK_SECTIONS
                    || record.start_page != expected_start
                    || record
                        .start_page
                        .checked_add(u32::from(record.page_count))
                        .is_none_or(|end| end > header.total_pages)
                    || header.next_spine.is_some_and(|next| record.spine >= next)
                    || (index + 1 == usize::from(header.section_count)
                        && record.start_page + u32::from(record.page_count) != header.total_pages)
                {
                    return false;
                }
                previous = Some(record);
                sections.push(record)
            },
        ) {
            library.book_sections.clear();
            return BookIndexLoadResult::Invalid;
        }
        if !read_seed_records(file, &header, library) {
            return invalid_index(library);
        }
        let mut toc = [EMPTY_TOC_RECORD; MAX_SD_TOC_ITEMS];
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
                toc[index] = record;
                true
            },
        ) {
            return invalid_index(library);
        }
        library.clear_toc();
        if header.toc_text_bytes > 0 {
            let text_len = header.toc_text_bytes as usize;
            if read_exact_file(file, &mut library.toc_text[..text_len]).is_err() {
                return invalid_index(library);
            }
            library.toc_text_len = text_len;
            library.toc_count = header.toc_count as usize;
            for (index, record) in toc
                .iter()
                .take(header.toc_count as usize)
                .copied()
                .enumerate()
            {
                library.toc[index] = record;
                library.toc_page[index] = 0;
            }
        }
        let mut title = [0u8; 64];
        let mut author = [0u8; 64];
        let mut title_str = "";
        let mut author_str = "";
        if header.title_text_bytes > 0 {
            let title_len = header.title_text_bytes as usize;
            if read_exact_file(file, &mut title[..title_len]).is_err() {
                return invalid_index(library);
            }
            let Ok(parsed_title) = core::str::from_utf8(&title[..title_len]) else {
                return invalid_index(library);
            };
            title_str = parsed_title;
        }
        if header.author_text_bytes > 0 {
            let author_len = header.author_text_bytes as usize;
            if read_exact_file(file, &mut author[..author_len]).is_err() {
                return invalid_index(library);
            }
            let Ok(parsed_author) = core::str::from_utf8(&author[..author_len]) else {
                return invalid_index(library);
            };
            author_str = parsed_author;
        }
        if header.title_text_bytes > 0 || header.author_text_bytes > 0 {
            library.set_book_labels(title_str, author_str);
        }
        library.commit_book_index(header.total_pages, header.partial);
        library.set_book_next_spine(header.next_spine);
        library.set_book_generated_toc(header.generated_toc);
        library.set_book_has_images(header.has_images);
        library.set_text_start_spine(header.text_start_spine);
        if book_v2_predates_images(&header_bytes) {
            BookIndexLoadResult::BeforeImages { spacing_changed }
        } else if spacing_changed {
            BookIndexLoadResult::SpacingMismatch
        } else {
            BookIndexLoadResult::Hit
        }
    })
    .unwrap_or(BookIndexLoadResult::Miss)
}

/// Settles a book index loaded as `BookIndexLoadResult::BeforeImages`,
/// once the book's ZIP has been checked for image entries (`None` when it
/// could not be read). This runs once per book: whatever it decides, the
/// book's next index is written as the current version.
///
/// - No images: the index is written again unchanged but for its version,
///   through the usual generation-safe write, and is a hit; with the
///   spacing changed, the reflow that follows writes it instead. Nothing
///   is rebuilt.
/// - Images: `page` is pinned in the old pagination and the index is
///   dropped, so the book rebuilds into the other bank (the old index
///   stays on the card until the new one commits) and its images replace
///   the alt text it was laid out with.
/// - Unread: a complete book is read as it stands and checked again at its
///   next open; one that would be built on (a prefix, or a spacing reflow)
///   rebuilds instead, so nothing ever extends an unchecked index.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_index_before_images<
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
    has_images: Option<bool>,
    spacing_changed: bool,
    page: u32,
    spine: Option<u16>,
) -> BookIndexLoadResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if has_images == Some(false) {
        library.set_book_has_images(false);
    }
    match has_images {
        Some(false) if spacing_changed => BookIndexLoadResult::SpacingMismatch,
        Some(false) => {
            if !write_v2_book_index(
                root,
                key,
                source_identity,
                library.book_total_pages,
                &library.book_sections,
                library,
                library.book_cache_partial,
            ) {
                // Still readable as it is; the next open tries again.
                esp_println::println!("cache: v27 index kept; the rewrite failed key={}", key);
            }
            BookIndexLoadResult::Hit
        }
        None if !spacing_changed && library.book_next_spine().is_none() => BookIndexLoadResult::Hit,
        Some(true) | None => {
            if spine.is_none() {
                library.anchor_resident_page(source_identity, page);
            }
            library.clear_book_index();
            BookIndexLoadResult::Invalid
        }
    }
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
    (|| {
        let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
        let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
        let book_dir = cache.open_dir(key).ok()?;
        // Labels are advisory EPUB metadata, independent of pagination. Most
        // catalog entries still have a legacy index: avoid probing both absent
        // generation files for every book at boot. Never use this ordering for
        // page totals, section loading, or bank retirement.
        if let Ok(file) = book_dir.open_file_in_dir(CACHE_BOOK_FILE, Mode::ReadOnly) {
            if read_book_title(&file, source_identity, out) {
                return Some(true);
            }
        }
        // The same source has the same EPUB title regardless of pagination.
        // Reading a label needs only its matching header and text: selecting a
        // generation would seek to both footers and walk their FAT chains.
        for name in book_generation::NAMES {
            if let Ok(file) = book_dir.open_file_in_dir(name, Mode::ReadOnly) {
                if read_book_title(&file, source_identity, out) {
                    return Some(true);
                }
            }
        }
        Some(false)
    })()
    .unwrap_or(false)
}

fn read_book_title<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    source_identity: (u32, u32),
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
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
}

/// Legacy subdir under XTEINK holding one `<8.3-stem>.TXT` label for books
/// uploaded by releases that predate VFAT long-name creation.
const LABELS_DIR: &str = "LABELS";

fn label_file_name(open_name: &str, out: &mut String<12>) {
    out.clear();
    let stem = open_name.split('.').next().unwrap_or(open_name);
    let _ = out.push_str(stem);
    let _ = out.push_str(".TXT");
}

/// Read a legacy uploaded book's stashed label into `out`. Returns false
/// (leaving `out` untouched) when the book has no sidecar.
pub(crate) fn read_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
    out: &mut String<64>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return false;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let Ok(file) = labels.open_file_in_dir(file_name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut buf = [0u8; 64];
    let Ok(read) = file.read(&mut buf) else {
        return false;
    };
    let Ok(text) = core::str::from_utf8(&buf[..read]) else {
        return false;
    };
    if text.is_empty() {
        return false;
    }
    out.clear();
    let _ = out.push_str(text);
    true
}

/// Remove an uploaded book's label sidecar, so a deleted book's name can't
/// mislabel a later upload that reuses the same 8.3 name.
pub(crate) fn delete_upload_label<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    open_name: &str,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return;
    };
    let Ok(labels) = xteink.open_dir(LABELS_DIR) else {
        return;
    };
    let mut file_name = String::<12>::new();
    label_file_name(open_name, &mut file_name);
    let _ = crate::sd_session::remove_file_reclaiming_clusters(&labels, file_name.as_str());
}

/// Delete a book's whole cache folder: every file in it (index generations,
/// TOC, cover, saved position) and its SECTIONS folder, then the folder
/// itself. Used only for orphans (books no longer on the card), so the saved
/// position goes too; leaving it behind kept the folder alive forever. True
/// when the folder is gone.
pub(crate) fn empty_cache_dir<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(xteink) = root.open_dir(CACHE_ROOT_DIR) else {
        return false;
    };
    let Ok(cache) = xteink.open_dir(CACHE_V2_DIR) else {
        return false;
    };
    {
        let Ok(book) = cache.open_dir(key) else {
            return false;
        };
        if let Ok(sections) = book.open_dir(CACHE_SECTIONS_DIR) {
            delete_all_files(&sections);
        }
        // The SECTIONS handle has dropped; the empty directory can go now.
        let _ = book.delete_file_in_dir(CACHE_SECTIONS_DIR);
        delete_all_files(&book);
    }
    // Likewise the book handle: closed by the scope above, deletable here.
    cache.delete_file_in_dir(key).is_ok()
}

/// Delete every plain file in `dir`, listing it in batches (a directory
/// iteration holds the volume lock, so names are collected first). Listing
/// beats probing every possible section name, which cost hundreds of
/// directory searches per folder.
fn delete_all_files<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    dir: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use core::fmt::Write;
    const BATCH: usize = 32;
    // Bounded: a folder holds at most two banks of sections plus a few files.
    for _ in 0..(2 * MAX_BOOK_SECTIONS / BATCH + 4) {
        let mut names: heapless::Vec<String<12>, BATCH> = heapless::Vec::new();
        let _ = dir.iterate_dir(|entry| {
            if entry.attributes.is_directory()
                || entry.attributes.is_volume()
                || entry.attributes.is_lfn()
            {
                return;
            }
            let mut name = String::<12>::new();
            if write!(name, "{}", entry.name).is_ok() {
                let _ = names.push(name);
            }
        });
        if names.is_empty() {
            return;
        }
        let mut removed_any = false;
        for name in &names {
            removed_any |= crate::sd_session::remove_file_reclaiming_clusters(dir, name.as_str());
        }
        if !removed_any {
            return;
        }
    }
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
    sections: &SectionTable,
    library: &ReaderStore,
    partial: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let seed = &library.seed;
    if (total_pages == 0) != sections.is_empty()
        || (sections.is_empty() && seed.sections.is_empty())
    {
        return false;
    }
    if ensure_v2_cache_dirs(root, key).is_err() {
        return false;
    }
    with_v2_book_writer(root, key, |file| {
        let toc_count = library
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(u16::MAX as usize);
        let title_text_bytes = library.title.len().min(64) as u32;
        let author_text_bytes = library.author.len().min(64) as u32;
        let header = BookV2Header {
            next_spine: library.book_next_spine(),
            generated_toc: library.book_generated_toc(),
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
            font_config: reader_layout::reader_layout_config(
                library.type_settings(),
                library.portrait(),
            ),
            custom_font_identity: library.custom_font_identity(),
            partial,
            text_start_spine: library.text_start_spine(),
            seed_first_spine: seed.first_spine,
            seed_section_count: seed.sections.len() as u16,
            seed_reaches_end: seed.first_spine.is_some() && seed.next_spine.is_none(),
            seed_capped: seed.capped,
            has_images: library.book_has_images(),
        };
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if encode_book_v2_header(header, &mut bytes).is_err() {
            return None;
        }
        let mut stage = WriteStage::checksummed(file);
        if stage.push(&bytes).is_err() {
            return None;
        }
        let mut record_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];
        for section in sections.iter().chain(seed.sections.iter()) {
            if encode_book_v2_section(section, &mut record_bytes).is_err()
                || stage.push(&record_bytes).is_err()
            {
                return None;
            }
        }
        let mut toc_bytes = [0u8; TOC_RECORD_BYTES];
        for record in library.toc.iter().take(toc_count).copied() {
            if encode_toc(record, &mut toc_bytes).is_err() || stage.push(&toc_bytes).is_err() {
                return None;
            }
        }
        if stage.flush().is_err() {
            return None;
        }
        if header.toc_text_bytes > 0
            && stage
                .push(&library.toc_text[..header.toc_text_bytes as usize])
                .is_err()
        {
            return None;
        }
        if header.title_text_bytes > 0
            && stage
                .push(&library.title.as_bytes()[..header.title_text_bytes as usize])
                .is_err()
        {
            return None;
        }
        if header.author_text_bytes > 0
            && stage
                .push(&library.author.as_bytes()[..header.author_text_bytes as usize])
                .is_err()
        {
            return None;
        }
        stage.flush().ok()?;
        stage.checksum
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

/// The index slots of the images page `page` of `section` shows, read from
/// its section file without loading it into the store, which keeps the
/// page on screen: the header, the page's record, its block records, the
/// head of each image block's token, and the text of each line holding
/// images (a few dozen bytes at a time), a few small reads in all. For the
/// pages near the reader's that the store does not hold. A section laid
/// out for other settings (its page records would not be this layout's),
/// or one that does not add up, lists nothing; the request for the page
/// still decodes what it shows. `visit` returning false stops the list.
pub(crate) fn read_section_page_images<D, T, const ND: usize, const NF: usize, const NV: usize>(
    book_dir: &Directory<'_, D, T, ND, NF, NV>,
    source_identity: (u32, u32),
    library: &ReaderStore,
    section: BookV2SectionRecord,
    page: u32,
    mut visit: impl FnMut(u16) -> bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Ok(sections) = book_dir.open_dir(CACHE_SECTIONS_DIR) else {
        return false;
    };
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(section.section, &mut name);
    let Ok(file) = sections.open_file_in_dir(name.as_str(), Mode::ReadOnly) else {
        return false;
    };
    let mut header_bytes = [0u8; SECTION_V2_HEADER_BYTES];
    if read_exact_file(&file, &mut header_bytes).is_err() {
        return false;
    }
    let Ok(header) = decode_section_v2_header(&header_bytes) else {
        return false;
    };
    let expected = reader_layout::reader_layout_config(library.type_settings(), library.portrait());
    let Some(local) = page
        .checked_sub(section.start_page)
        .filter(|&local| local < u32::from(header.page_count))
    else {
        return false;
    };
    if header.source_hash != source_identity.0
        || header.source_size != source_identity.1
        || header.spine != section.spine
        || header.custom_font_identity != library.custom_font_identity()
        || header.font_config != expected
    {
        return false;
    }
    let pages_at = SECTION_V2_HEADER_BYTES as u32;
    let blocks_at = pages_at + u32::from(header.page_count) * PAGE_RECORD_BYTES as u32;
    let text_at = blocks_at + u32::from(header.block_count) * (BLOCK_RECORD_BYTES as u32 + 1);
    let mut stage = [0u8; RECORD_STAGE_BYTES];
    let page_record = &mut stage[..PAGE_RECORD_BYTES];
    if file
        .seek_from_start(pages_at + local * PAGE_RECORD_BYTES as u32)
        .is_err()
        || read_exact_file(&file, page_record).is_err()
    {
        return false;
    }
    let Ok(page_record) = decode_page(page_record) else {
        return false;
    };
    let first = u32::from(page_record.first_block);
    let end = first + u32::from(page_record.block_count);
    if end > u32::from(header.block_count) {
        return false;
    }
    let per_batch = (RECORD_STAGE_BYTES / BLOCK_RECORD_BYTES) as u32;
    let mut block = first;
    while block < end {
        let count = (end - block).min(per_batch);
        let records = &mut stage[..count as usize * BLOCK_RECORD_BYTES];
        if file
            .seek_from_start(blocks_at + block * BLOCK_RECORD_BYTES as u32)
            .is_err()
            || read_exact_file(&file, records).is_err()
        {
            return false;
        }
        for bytes in records.chunks_exact(BLOCK_RECORD_BYTES) {
            let Ok(record) = decode_block(bytes) else {
                return false;
            };
            if record.has_inline_images() {
                match visit_line_images(&file, text_at, header.text_bytes, record, &mut visit) {
                    Some(true) => continue,
                    Some(false) => return true,
                    None => return false,
                }
            }
            let head_end = record
                .text_offset
                .checked_add(proto::image::token::IMAGE_TOKEN_HEAD_BYTES as u32);
            let head_at = text_at.checked_add(record.text_offset);
            let (true, Some(head_at)) = (
                record.is_image() && head_end.is_some_and(|end| end <= header.text_bytes),
                head_at,
            ) else {
                continue;
            };
            let mut head = [0u8; proto::image::token::IMAGE_TOKEN_HEAD_BYTES];
            if file.seek_from_start(head_at).is_err() || read_exact_file(&file, &mut head).is_err()
            {
                return false;
            }
            let token = core::str::from_utf8(&head)
                .ok()
                .and_then(proto::image::token::decode_image_token);
            if let Some(token) = token {
                if !visit(token.slot) {
                    return true;
                }
            }
        }
        block += count;
    }
    true
}

/// Calls `visit` with the slot of each image inside a line of a section
/// file whose text starts at `text_at` (`text_bytes` long), reading the
/// line a few dozen bytes at a time. `Some(false)` when `visit` stopped,
/// `None` when the line does not lie inside the text or a read fails.
fn visit_line_images<D, T, const ND: usize, const NF: usize, const NV: usize>(
    file: &File<'_, D, T, ND, NF, NV>,
    text_at: u32,
    text_bytes: u32,
    record: proto::cache::BlockRecord,
    visit: &mut impl FnMut(u16) -> bool,
) -> Option<bool>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    use proto::image::token::{inline_image_token_at, INLINE_IMAGE_TOKEN_BYTES};
    const PIECE: usize = 64;
    let end = record.text_offset.checked_add(u32::from(record.text_len))?;
    if end > text_bytes {
        return None;
    }
    let mut piece = [0u8; PIECE];
    let mut at = record.text_offset;
    while at < end {
        let len = (end - at).min(PIECE as u32) as usize;
        let bytes = &mut piece[..len];
        file.seek_from_start(text_at.checked_add(at)?).ok()?;
        read_exact_file(file, bytes).ok()?;
        // Read on from where a token could still begin, so one cut by the
        // piece's end is read whole with the next.
        let last = end - at <= PIECE as u32;
        let scan = if last {
            len
        } else {
            len - (INLINE_IMAGE_TOKEN_BYTES - 1)
        };
        let mut index = 0;
        let mut next = scan;
        while index < scan {
            match inline_image_token_at(&bytes[index..]) {
                Some(token) => {
                    if !visit(token.slot) {
                        return Some(false);
                    }
                    index += INLINE_IMAGE_TOKEN_BYTES;
                    next = next.max(index);
                }
                None => index += 1,
            }
        }
        at += next as u32;
    }
    Some(true)
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
    if ensure_v2_cache_dirs(root, key).is_err() {
        esp_println::println!("cache: v2 ensure dirs failed key={}", key);
        return f(None);
    }
    // One handle walks the chain via change_dir, so the whole build holds a
    // single directory slot instead of the four-level ladder.
    let Ok(mut dir) = root.open_dir(CACHE_ROOT_DIR) else {
        return f(None);
    };
    if dir.change_dir(CACHE_V2_DIR).is_err()
        || dir.change_dir(key).is_err()
        || dir.change_dir(CACHE_SECTIONS_DIR).is_err()
    {
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
    let name = book_generation::selected(&book_dir, false)
        .map(|(slot, _)| book_generation::NAMES[slot])
        .unwrap_or(CACHE_BOOK_FILE);
    let file = book_dir.open_file_in_dir(name, mode).ok()?;
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
    checksum: Option<u32>,
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
            checksum: None,
        }
    }

    fn checksummed(file: &'f File<'v, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>) -> Self {
        let mut stage = Self::new(file);
        stage.checksum = Some(0x811c_9dc5);
        stage
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if let Some(hash) = self.checksum.as_mut() {
            *hash = bytes
                .iter()
                .fold(*hash, |h, b| (h ^ u32::from(*b)).wrapping_mul(0x0100_0193));
        }
        if bytes.len() > self.buf.len() - self.len {
            self.flush()?;
        }
        if bytes.len() >= self.buf.len() {
            return self.file.write(bytes).map_err(|_| ());
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.len == 0 {
            return Ok(());
        }
        let result = self.file.write(&self.buf[..self.len]).map_err(|_| ());
        self.len = 0;
        result
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

/// Fills `out` from the file's current position; anything short of that
/// is an error. The one read loop every card file goes through.
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

fn with_verified_v2_book_file<R, D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    f: impl FnOnce(&File<'_, D, T, ND, NF, NV>) -> R,
) -> Option<R>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let dir = cache.open_dir(key).ok()?;
    let name = book_generation::selected_checked(&dir, true)
        .ok()?
        .map(|(slot, _)| book_generation::NAMES[slot])
        .unwrap_or(CACHE_BOOK_FILE);
    let file = dir.open_file_in_dir(name, Mode::ReadOnly).ok()?;
    Some(f(&file))
}

fn with_v2_book_writer<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    f: impl FnOnce(&File<'_, D, T, ND, NF, NV>) -> Option<u32>,
) -> Option<bool>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let dir = cache.open_dir(key).ok()?;
    Some(book_generation::write(&dir, f))
}

/// Select the inactive section bank and retire any index that still references
/// it BEFORE its files are overwritten. The active generation stays readable
/// until the replacement's complete, checksummed index has been published.
pub(crate) fn prepare_book_replacement<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
) -> Option<u16>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    ensure_v2_cache_dirs(root, key).ok()?;
    let xteink = root.open_dir(CACHE_ROOT_DIR).ok()?;
    let cache = xteink.open_dir(CACHE_V2_DIR).ok()?;
    let dir = cache.open_dir(key).ok()?;
    let selected = book_generation::selected_checked(&dir, true)
        .ok()?
        .map(|(slot, _)| book_generation::NAMES[slot])
        .unwrap_or(CACHE_BOOK_FILE);
    let active = index_section_bank(&dir, selected).ok()?;
    let target = if active == Some(0) {
        MAX_BOOK_SECTIONS as u16
    } else {
        0
    };
    for name in [
        CACHE_BOOK_FILE,
        book_generation::NAMES[0],
        book_generation::NAMES[1],
    ] {
        if active.is_some() && name == selected {
            continue;
        }
        match dir.find_directory_entry(name) {
            Ok(_) => {
                if !crate::sd_session::remove_file_reclaiming_clusters(&dir, name) {
                    return None;
                }
            }
            Err(embedded_sdmmc::Error::NotFound) => {}
            Err(_) => return None,
        }
    }
    Some(target)
}

fn index_section_bank<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    name: &str,
) -> Result<Option<u16>, ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let file = match dir.open_file_in_dir(name, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut bytes = [0; BOOK_V2_HEADER_BYTES];
    read_exact_file(&file, &mut bytes).map_err(|_| ())?;
    let Ok(header) = decode_book_v2_header(&bytes) else {
        return Ok(None);
    };
    if header.section_count == 0 && header.seed_section_count == 0 {
        return Ok(None);
    }
    let mut record = [0; BOOK_V2_SECTION_RECORD_BYTES];
    read_exact_file(&file, &mut record).map_err(|_| ())?;
    let Ok(record) = decode_book_v2_section(&record) else {
        return Ok(None);
    };
    let first = usize::from(record.section);
    if first < MAX_BOOK_SECTIONS * 2 {
        return Ok(Some((first / MAX_BOOK_SECTIONS * MAX_BOOK_SECTIONS) as u16));
    }
    // Only a seed run so far: its ids say which bank the prefix will use.
    let seed = first - usize::from(SEED_SECTION_BASE);
    Ok((seed < 2 * MAX_SEED_SECTIONS)
        .then_some((seed / MAX_SEED_SECTIONS * MAX_BOOK_SECTIONS) as u16))
}

/// Reflow prewrapped lines through the same page plan, joining old section
/// boundaries before choosing new ones. Scratch is the idle XHTML buffer, not
/// additional stack/static storage. The verified old index remains recoverable
/// until every replacement section and its new index have reached the card.
#[inline(never)]
pub(crate) fn reflow_book_spacing<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    identity: (u32, u32),
    library: &mut ReaderStore,
    scratch: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if library.book_next_spine().is_some()
        || library.book_cache_partial
        || library.book_sections.is_empty()
    {
        return false;
    }
    // The replacement is written into the store's table while the old one
    // is read, and it can run longer (more sections) than the old, so the
    // old table is read from a copy: 4 KB at most, compact as resident.
    let original_sections = library.book_sections;
    let Some(base) = prepare_book_replacement(root, key) else {
        return false;
    };
    library.book_cache_ready = false;
    library.clear_lines();
    library.book_sections.clear();
    // New spacing, new pages: a seed run's provisional numbers no longer
    // map onto them. Requests in flight were pinned before the reflow.
    library.seed.clear();
    let mut count = 0usize;
    let mut pages = 0u32;
    let mut previous_spine = None;
    let ok = with_v2_sections_dir(root, key, |dir| {
        let Some(dir) = dir else {
            return false;
        };
        for section in original_sections.iter() {
            if previous_spine.is_some_and(|spine| spine != section.spine)
                && !flush_reflow_window(dir, identity, base, library, &mut count, &mut pages, false)
            {
                return false;
            }
            previous_spine = Some(section.spine);
            let Some(header) = read_reflow_source(dir, &section, identity, library, scratch) else {
                return false;
            };
            let blocks_at =
                SECTION_V2_HEADER_BYTES + usize::from(header.page_count) * PAGE_RECORD_BYTES;
            let flags_at = blocks_at + usize::from(header.block_count) * BLOCK_RECORD_BYTES;
            let text_at = flags_at + usize::from(header.block_count);
            for index in 0..usize::from(header.block_count) {
                let record_at = blocks_at + index * BLOCK_RECORD_BYTES;
                let Ok(block) = decode_block(&scratch[record_at..record_at + BLOCK_RECORD_BYTES])
                else {
                    return false;
                };
                let start = block.text_offset as usize;
                let Some(end) = start.checked_add(usize::from(block.text_len)) else {
                    return false;
                };
                if block.line_count != 1
                    || end > header.text_bytes as usize
                    || block.text_len as usize > crate::reader_store::MAX_READER_BLOCK_TEXT
                {
                    return false;
                }
                let Ok(text) = core::str::from_utf8(&scratch[text_at + start..text_at + end])
                else {
                    return false;
                };
                if text.is_empty() || text.trim_matches(|ch: char| ch.is_ascii_whitespace()) != text
                {
                    return false;
                }
                let flags = scratch[flags_at + index];
                if flags & !0b11 != 0 {
                    return false;
                }
                // Leave the same worst-case line headroom as the XHTML sink.
                if (library.block_count() >= library.block_capacity().saturating_sub(4)
                    || library.text_capacity_reached()
                    || library.page_count >= library.page_capacity())
                    && !flush_reflow_window(
                        dir, identity, base, library, &mut count, &mut pages, true,
                    )
                {
                    return false;
                }
                let next = library.block_count();
                if !library.push_line_block(
                    text,
                    display_style_for_proto_style(block.style),
                    block.role,
                    block.align,
                    flags & 1 != 0,
                    section.spine,
                ) || library.block_count() != next + 1
                {
                    return false;
                }
                // An image keeps its size, which no spacing changes.
                library.blocks[next].image_height = block.image_height;
                // The old section may start in the middle of a paragraph.
                library.block_paragraph_start[next] = flags & 2 != 0;
                reader_layout::extend_page_index(library);
            }
        }
        flush_reflow_window(dir, identity, base, library, &mut count, &mut pages, false)
    });
    if !ok || count == 0 {
        return false;
    }
    library.book_total_pages = pages;
    reader_layout::rebuild_toc_page_targets(library);
    library.chapter_page_count = 0;
    if !write_v2_book_index(
        root,
        key,
        identity,
        pages,
        &library.book_sections,
        library,
        false,
    ) {
        return false;
    }
    library.book_total_pages = pages;
    library.book_cache_ready = true;
    library.book_cache_partial = false;
    true
}

fn read_reflow_source<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    section: &BookV2SectionRecord,
    identity: (u32, u32),
    library: &ReaderStore,
    scratch: &mut [u8],
) -> Option<SectionV2Header>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(section.section, &mut name);
    let file = dir.open_file_in_dir(name.as_str(), Mode::ReadOnly).ok()?;
    let bytes = scratch.get_mut(..SECTION_V2_HEADER_BYTES)?;
    read_exact_file(&file, bytes).ok()?;
    let header = decode_section_v2_header(bytes).ok()?;
    let config = reader_layout::reader_layout_config(library.type_settings(), library.portrait());
    if (header.source_hash, header.source_size) != identity
        || header.spine != section.spine
        || header.custom_font_identity != library.custom_font_identity()
        || header.font_config & !0b11 != config & !0b11
        || header.page_count != section.page_count
        || header.block_count == 0
        || header.partial
        || !library.can_hold_section(
            header.page_count as usize,
            header.block_count as usize,
            header.text_bytes as usize,
        )
    {
        return None;
    }
    let len = proto::cache::section_v2_cache_size(header);
    if file.length() as usize != len {
        return None;
    }
    read_exact_file(&file, scratch.get_mut(SECTION_V2_HEADER_BYTES..len)?).ok()?;
    Some(header)
}

fn flush_reflow_window<D, T, const ND: usize, const NF: usize, const NV: usize>(
    dir: &Directory<'_, D, T, ND, NF, NV>,
    identity: (u32, u32),
    base: u16,
    library: &mut ReaderStore,
    count: &mut usize,
    total: &mut u32,
    carry: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if library.block_count() == 0 {
        return true;
    }
    if *count >= MAX_BOOK_SECTIONS {
        return false;
    }
    let id = base + *count as u16;
    let spine = library.block_spine[0];
    library.set_cached_spine(spine);
    let mut written_pages = 0;
    let ok = reader_layout::flush_page_window(library, carry, |window| {
        written_pages = window.page_count;
        write_v2_section_cache_in(dir, identity, id, window)
    });
    if !ok {
        return false;
    }
    if !library.book_sections.push(BookV2SectionRecord {
        section: id,
        spine,
        start_page: *total,
        page_count: written_pages as u16,
        partial: false,
    }) {
        return false;
    }
    *count += 1;
    *total += written_pages as u32;
    true
}
