use crate::reader_layout;
use crate::reader_store::{
    ReaderStore, EMPTY_BOOK_SECTION_RECORD, EMPTY_TOC_RECORD, MAX_BOOK_SECTIONS, MAX_SD_TOC_ITEMS,
    MAX_SD_TOC_TEXT_BYTES,
};
use display::font::FontStyle;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use heapless::String;
use proto::cache::{
    decode_block, decode_book_v2_header, decode_book_v2_section, decode_cover_header, decode_page,
    decode_section_header, decode_section_v2_header, decode_toc, encode_block,
    encode_book_v2_header, encode_book_v2_section, encode_page, encode_section_v2_header,
    encode_toc, section_file_name, BookV2Header, BookV2SectionRecord, SectionV2Header,
    BLOCK_RECORD_BYTES, BOOK_V2_HEADER_BYTES, BOOK_V2_SECTION_RECORD_BYTES, CACHE_BOOK_FILE,
    CACHE_COVER_FILE, CACHE_DIR, CACHE_ROOT_DIR, CACHE_SECTIONS_DIR, CACHE_SECTION_FILE_BYTES,
    CACHE_STATE_FILE, CACHE_V2_DIR, COVER_HEADER_BYTES, PAGE_RECORD_BYTES, SECTION_HEADER_BYTES,
    SECTION_V2_HEADER_BYTES, TOC_RECORD_BYTES,
};

const MIGRATE_MAX_SECTIONS: u16 = 16;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MigrationResult {
    Migrated,
    Skipped,
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
    let file = xteink
        .open_file_in_dir(CACHE_STATE_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| ())?;
    file.write(&record.encode()).map_err(|_| ())
}

/// Read and decode `/XTEINK/STATE.BIN`. Returns None when the directory
/// or file is missing, short, or fails the record checksum.
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
    let file = xteink
        .open_file_in_dir(CACHE_STATE_FILE, Mode::ReadOnly)
        .ok()?;
    let mut bytes = [0u8; hal_ext::nvm::AppStateRecord::ENCODED_LEN];
    // One read suffices for a 32-byte record; shorter V1/V2 files decode
    // from their actual length.
    let len = file.read(&mut bytes).ok()?;
    hal_ext::nvm::AppStateRecord::decode(&bytes[..len])
}

const WIFI_FILE: &str = "WIFI.BIN";

/// Write the onboarding portal's captured credentials to /XTEINK/WIFI.BIN.
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
    let file = xteink
        .open_file_in_dir(WIFI_FILE, Mode::ReadWriteCreateOrTruncate)
        .map_err(|_| ())?;
    file.write(&record.encode()).map_err(|_| ())
}

/// Read /XTEINK/WIFI.BIN; None when missing, short, or corrupt.
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
    let file = xteink.open_file_in_dir(WIFI_FILE, Mode::ReadOnly).ok()?;
    let mut bytes = [0u8; hal_ext::nvm::WifiCredentialsRecord::ENCODED_LEN];
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
            || header.section_count as usize > MAX_BOOK_SECTIONS
            || header.toc_count as usize > MAX_SD_TOC_ITEMS
            || header.toc_text_bytes as usize > MAX_SD_TOC_TEXT_BYTES
            || header.title_text_bytes as usize > 64
            || header.author_text_bytes as usize > 64
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
            return BookIndexLoadResult::Invalid;
        }
        library.clear_toc();
        if header.toc_text_bytes > 0 {
            let text_len = header.toc_text_bytes as usize;
            if read_exact_file(file, &mut library.toc_text[..text_len]).is_err() {
                return BookIndexLoadResult::Invalid;
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
                return BookIndexLoadResult::Invalid;
            }
            let Ok(parsed_title) = core::str::from_utf8(&title[..title_len]) else {
                return BookIndexLoadResult::Invalid;
            };
            title_str = parsed_title;
        }
        if header.author_text_bytes > 0 {
            let author_len = header.author_text_bytes as usize;
            if read_exact_file(file, &mut author[..author_len]).is_err() {
                return BookIndexLoadResult::Invalid;
            }
            let Ok(parsed_author) = core::str::from_utf8(&author[..author_len]) else {
                return BookIndexLoadResult::Invalid;
            };
            author_str = parsed_author;
        }
        if header.title_text_bytes > 0 || header.author_text_bytes > 0 {
            library.set_book_labels(title_str, author_str);
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
            font_config: reader_layout::reader_layout_config(library.type_settings()),
            partial,
        };
        let mut bytes = [0u8; BOOK_V2_HEADER_BYTES];
        if encode_book_v2_header(header, &mut bytes).is_err() || file.write(&bytes).is_err() {
            return false;
        }
        let mut record_bytes = [0u8; BOOK_V2_SECTION_RECORD_BYTES];
        for section in sections {
            if encode_book_v2_section(*section, &mut record_bytes).is_err()
                || file.write(&record_bytes).is_err()
            {
                return false;
            }
        }
        let mut toc_bytes = [0u8; TOC_RECORD_BYTES];
        for record in library.toc.iter().take(toc_count).copied() {
            if encode_toc(record, &mut toc_bytes).is_err() || file.write(&toc_bytes).is_err() {
                return false;
            }
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
        let expected_config = reader_layout::reader_layout_config(library.type_settings());
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
            reader_layout::rebuild_page_index(
                library,
                reader_layout::READER_PAGE_TOP,
                reader_layout::READER_PAGE_BOTTOM,
            );
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

pub(crate) fn migrate_v1_cache_for_entry<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
) -> MigrationResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut migrated = false;
    let mut invalid = false;
    for spine in 0..MIGRATE_MAX_SECTIONS {
        if with_v2_section_file(root, key, spine, Mode::ReadOnly, |_| ()).is_some() {
            continue;
        }
        let Some(result) = with_v1_section_file(root, key, spine, Mode::ReadOnly, |v1_file| {
            migrate_v1_section_file(root, key, source_identity, spine, v1_file)
        }) else {
            continue;
        };
        match result {
            MigrationResult::Migrated => migrated = true,
            MigrationResult::Invalid => invalid = true,
            MigrationResult::Skipped => {}
        }
    }
    if migrated {
        MigrationResult::Migrated
    } else if invalid {
        MigrationResult::Invalid
    } else {
        // saw_v1 and the no-state case both land on Skipped.
        MigrationResult::Skipped
    }
}

fn migrate_v1_section_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    key: &str,
    source_identity: (u32, u32),
    spine: u16,
    v1_file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> MigrationResult
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut header_bytes = [0u8; SECTION_HEADER_BYTES];
    if read_exact_file(v1_file, &mut header_bytes).is_err() {
        return MigrationResult::Invalid;
    }
    let Ok(v1) = decode_section_header(&header_bytes) else {
        return MigrationResult::Invalid;
    };
    let body_bytes = v1.page_count as usize * PAGE_RECORD_BYTES
        + v1.block_count as usize * BLOCK_RECORD_BYTES
        + v1.block_count as usize
        + v1.text_bytes as usize;
    if body_bytes > 24_576 {
        return MigrationResult::Invalid;
    }
    if ensure_v2_cache_dirs(root, key).is_err() {
        return MigrationResult::Skipped;
    }
    let v2 = SectionV2Header {
        source_hash: source_identity.0,
        source_size: source_identity.1,
        spine,
        page_count: v1.page_count,
        block_count: v1.block_count,
        text_bytes: v1.text_bytes,
        viewport_width: v1.viewport_width,
        viewport_height: v1.viewport_height,
        font_config: v1.font_config,
        bytes_consumed: v1.bytes_consumed,
        total_bytes: v1.total_bytes,
        partial: v1.partial,
    };
    with_v2_section_file(
        root,
        key,
        spine,
        Mode::ReadWriteCreateOrTruncate,
        |v2_file| {
            let mut v2_header = [0u8; SECTION_V2_HEADER_BYTES];
            if encode_section_v2_header(v2, &mut v2_header).is_err()
                || v2_file.write(&v2_header).is_err()
            {
                return MigrationResult::Skipped;
            }
            if copy_file_bytes(v1_file, v2_file, body_bytes).is_err() {
                return MigrationResult::Invalid;
            }
            MigrationResult::Migrated
        },
    )
    .unwrap_or(MigrationResult::Skipped)
}

fn with_v1_section_file<
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
    let cache = xteink.open_dir(CACHE_DIR).ok()?;
    let book_dir = cache.open_dir(key).ok()?;
    let sections = book_dir.open_dir(CACHE_SECTIONS_DIR).ok()?;
    let mut name = String::<CACHE_SECTION_FILE_BYTES>::new();
    section_file_name(spine, &mut name);
    let file = sections.open_file_in_dir(name.as_str(), mode).ok()?;
    Some(f(&file))
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
        library.set_cached_paragraph_end(index, bytes[0] != 0)
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
        font_config: reader_layout::reader_layout_config(library.type_settings()),
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
    for page in library.pages.iter().take(library.page_count) {
        if encode_page(*page, &mut record[..PAGE_RECORD_BYTES]).is_err()
            || file.write(&record[..PAGE_RECORD_BYTES]).is_err()
        {
            esp_println::println!("cache: write page record failed");
            return false;
        }
    }
    for block in library.blocks.iter().take(library.block_count) {
        if encode_block(*block, &mut record[..BLOCK_RECORD_BYTES]).is_err()
            || file.write(&record[..BLOCK_RECORD_BYTES]).is_err()
        {
            esp_println::println!("cache: write block record failed");
            return false;
        }
    }
    for flag in library
        .block_paragraph_end
        .iter()
        .take(library.block_count)
        .copied()
    {
        if file.write(&[flag as u8]).is_err() {
            esp_println::println!("cache: write paragraph flag failed");
            return false;
        }
    }
    if file.write(&library.text[..library.text_len]).is_err() {
        esp_println::println!("cache: write text failed");
        return false;
    }
    true
}

/// Staging size for batched record reads. Kept small: this sits on the
/// stack inside the EPUB open path, alongside copy_file_bytes' 512-byte
/// scratch in the same budget region.
const RECORD_STAGE_BYTES: usize = 256;

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

fn read_exact_file<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
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

fn copy_file_bytes<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    source: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    target: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    mut bytes: usize,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut scratch = [0u8; 512];
    while bytes > 0 {
        let count = bytes.min(scratch.len());
        read_exact_file(source, &mut scratch[..count])?;
        target.write(&scratch[..count]).map_err(|_| ())?;
        bytes -= count;
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
