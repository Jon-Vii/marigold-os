use crate::display_flush::Epd;
use crate::reader_cache_files;
use crate::reader_cache_files::{BookIndexLoadResult, CacheLoadResult};
use crate::reader_layout;
use crate::reader_store::{
    source_hash, BookLoadStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
    MAX_READER_BLOCK_TEXT,
};
use crate::sd_session::{self, SdSessionError};
use display::font::{fixed_ceil, fixed_round, FontFamily, FontStyle, TypeSettings, STYLE_MARKER};
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;
use heapless::String;
use proto::book::BookId;
use proto::cache::{BookV2SectionRecord, CONTENT_HEADER_BYTES};
use proto::epub::{
    decode_html_entity, parse_opf, strip_fragment, CssRules, Epub3NavStreamParser, EpubTocSink,
    EpubZipOps, NcxStreamParser, ReadAt, StreamingXmlTokenizer, TocError, XhtmlBlockSink,
    XhtmlBlockStreamParser, XhtmlError, ZipInflateScratch, ZipStream, MAX_ENTRY_NAME_BYTES,
};
use proto::text::{TextAlign, TextRole};
use ui::reading::StyledInkCursor;

pub(crate) const READER_TAIL_SCRATCH: usize = 4096;
pub(crate) const READER_HEADER_SCRATCH: usize = 46;
// All zip reads stream through the shared inflate engine in bounded chunks,
// so this only sets the fetch granularity; SD transfers are 2 KB ops either
// way. Kept small so the freed static RAM widens the stack region.
pub(crate) const READER_COMPRESSED_SCRATCH: usize = 8192;
pub(crate) const READER_CONTAINER_SCRATCH: usize = 4096;
pub(crate) const READER_OPF_SCRATCH: usize = 16_384;
pub(crate) const READER_XHTML_SCRATCH: usize = 24_576;
/// Per-call clamp on `read_at`. The FAT layer transfers single 512-byte
/// blocks either way, so this only bounds how much one embedded-sdmmc call
/// copies; matching the compressed scratch keeps an 8 KB inflate fetch to
/// one seek + one read call instead of four.
const EPUB_READ_AT_CHUNK_BYTES: usize = 8192;
const EPUB_OPEN_READ_OP_LIMIT: u32 = 65_536;
const EPUB_OPEN_READ_BYTE_LIMIT: u32 = 64 * 1024 * 1024;

pub(crate) struct ReaderCacheScratch<'a> {
    tail: &'a mut [u8; READER_TAIL_SCRATCH],
    header: &'a mut [u8; READER_HEADER_SCRATCH],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    container: &'a mut [u8; READER_CONTAINER_SCRATCH],
    opf: &'a mut [u8; READER_OPF_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    zip_inflate: ZipInflateScratch,
}

struct TocScratch<'a> {
    header: &'a mut [u8; 46],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    zip_inflate: &'a mut ZipInflateScratch,
}

struct LibraryTocSink<'a, 'p> {
    library: &'a mut ReaderStore,
    package: &'p proto::epub::EpubPackage<'p>,
    /// The full chapter list streams into this scratch buffer as fixed-size
    /// records, then gets written to TOC.BIN. Holds up to
    /// `buf.len() / TOC_CHAPTER_RECORD_BYTES` chapters.
    toc_buf: &'a mut [u8],
    record_count: usize,
    resident_full: bool,
}

impl EpubTocSink for LibraryTocSink<'_, '_> {
    fn push_toc(&mut self, title: &str, href: &str, level: u8) -> Result<(), TocError> {
        let spine_index = self
            .package
            .spine
            .iter()
            .position(|item| href_matches_spine(href, item.href.of(self.package.opf_text)))
            .map(|index| index as i16)
            .unwrap_or(-1);
        // Stream the full chapter list (uncapped up to the scratch buffer)
        // into fixed-size records for TOC.BIN.
        let offset = self.record_count * proto::cache::TOC_CHAPTER_RECORD_BYTES;
        if offset + proto::cache::TOC_CHAPTER_RECORD_BYTES <= self.toc_buf.len() {
            let record = proto::cache::toc_chapter_record(title, level, spine_index);
            if proto::cache::encode_toc_chapter(
                &record,
                &mut self.toc_buf[offset..offset + proto::cache::TOC_CHAPTER_RECORD_BYTES],
            )
            .is_ok()
            {
                self.record_count += 1;
            }
        }
        // The resident copy still feeds the current (capped) overview until
        // stage 2 switches it to the on-disk list.
        if !self.resident_full && !self.library.push_toc_record(title, level, spine_index) {
            self.resident_full = true;
        }
        Ok(())
    }
}

impl<'a> ReaderCacheScratch<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tail: &'a mut [u8; READER_TAIL_SCRATCH],
        header: &'a mut [u8; READER_HEADER_SCRATCH],
        name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
        compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
        container: &'a mut [u8; READER_CONTAINER_SCRATCH],
        opf: &'a mut [u8; READER_OPF_SCRATCH],
        xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
        book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    ) -> Self {
        Self {
            tail,
            header,
            name,
            compressed,
            container,
            opf,
            xhtml,
            book_sections,
            zip_inflate: ZipInflateScratch::new(),
        }
    }
}

/// Tears the built scratch down into the raw regions the sync session
/// loans to the radio. One-way: the regions alias the scratch's borrowed
/// arrays and its own struct storage (the inflate state is the bulk of
/// it), so the scratch must never be used as a scratch again — only the
/// session-ending software reset brings the reader pipeline back.
#[allow(unsafe_code)]
pub(crate) fn dismantle_scratch(
    scratch: &'static mut ReaderCacheScratch<'static>,
) -> crate::sync_mem::SyncLoan {
    use crate::sync_mem::{RawRegion, SyncLoan};

    // Raw field pointers first; they chain provenance through the field
    // borrows into the separate backing statics, not into the struct.
    let xhtml = RawRegion {
        ptr: scratch.xhtml.as_mut_ptr(),
        len: READER_XHTML_SCRATCH,
    };
    let opf_ptr = scratch.opf.as_mut_ptr();
    let compressed_ptr = scratch.compressed.as_mut_ptr();
    let container_ptr = scratch.container.as_mut_ptr();
    let tail_ptr = scratch.tail.as_mut_ptr();

    // The struct's own storage becomes a heap region, dead references,
    // padding, and all. Nothing reads it as a struct afterwards.
    let struct_region = RawRegion {
        ptr: (scratch as *mut ReaderCacheScratch<'static>).cast::<u8>(),
        len: core::mem::size_of::<ReaderCacheScratch<'static>>(),
    };

    // Safety: each pointer addresses a distinct 'static allocation whose
    // only other path is the scratch struct this function retires.
    unsafe {
        SyncLoan {
            heap_a: struct_region,
            heap_b: xhtml,
            tcp_rx: core::slice::from_raw_parts_mut(opf_ptr, READER_OPF_SCRATCH),
            tcp_tx: core::slice::from_raw_parts_mut(compressed_ptr, READER_COMPRESSED_SCRATCH),
            http_a: core::slice::from_raw_parts_mut(container_ptr, READER_CONTAINER_SCRATCH),
            http_b: core::slice::from_raw_parts_mut(tail_ptr, READER_TAIL_SCRATCH),
            wifi: None,
            catalog_len: 0,
        }
    }
}

/// Kept out of line: the storage dispatcher's frame must stay small, and the
/// EPUB open path below already runs close to the 30 KB stack region.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_or_load_book_cache(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    requested_chapter: u8,
    target_pages: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) {
    esp_println::println!(
        "epub: cache open index {} chapter {} target {}",
        index,
        requested_chapter,
        target_pages
    );
    library.begin_book_load();

    if library.catalog_entry(index).is_none() {
        set_preview_error(library, "BAD INDEX");
        library.set_reader_status(BookLoadStatus::Error);
        return;
    }

    let status = sd_session::with_root(epd, sd_cs, |root| {
        build_or_load_book_cache_from_root(
            root,
            library,
            index,
            requested_chapter,
            target_pages,
            scratch,
            font_metrics,
        )
    })
    .unwrap_or_else(|err| {
        esp_println::println!("epub: session failed: {:?}", err);
        set_preview_error(library, session_error_label(err));
        BookLoadStatus::Error
    });

    library.finish_book_load(index, requested_chapter, status);
}

#[inline(never)]
pub(crate) fn build_or_load_book_cache_from_root<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &mut ReaderStore,
    index: usize,
    requested_chapter: u8,
    target_pages: usize,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> BookLoadStatus
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    esp_println::println!("epub: card init begin");
    esp_println::println!("epub: open root");
    let mut open_name = String::<16>::new();
    let mut display_name = String::<64>::new();
    let Some(entry) = library.catalog_entry(index) else {
        return BookLoadStatus::Error;
    };
    let in_books_dir = entry.in_books_dir;
    let source_identity = (entry.source_hash, entry.byte_size);
    let _ = open_name.push_str(&entry.open_name);
    let _ = display_name.push_str(&entry.display_name);
    esp_println::println!(
        "epub: catalog entry display='{}' open='{}' books={}",
        display_name,
        open_name,
        in_books_dir
    );
    let cache_key = proto::cache::cache_key_for(display_name.as_str(), source_identity.1);
    library.set_cache_key(cache_key.as_str());
    esp_println::println!("epub: stage ResolveCatalogEntry key={}", cache_key.as_str());
    esp_println::println!(
        "epub: stage TryV2BookIndexFast page={}",
        target_pages as u32
    );
    let status = if try_load_v2_book_cache(
        root,
        cache_key.as_str(),
        source_identity,
        target_pages as u32,
        library,
        Instant::now(),
        "fast",
    ) || try_replay_content_cache(
        root,
        cache_key.as_str(),
        source_identity,
        target_pages as u32,
        library,
        scratch,
        font_metrics,
    ) {
        BookLoadStatus::Ready
    } else if in_books_dir {
        let load_result = match root.open_dir("BOOKS") {
            Ok(books) => match books.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
                Ok(file) => Some(build_or_load_epub_cache_from_file(
                    file,
                    root,
                    &display_name,
                    requested_chapter,
                    target_pages,
                    library,
                    scratch,
                    font_metrics,
                )),
                Err(err) => {
                    esp_println::println!("epub: open file failed: {:?}", err);
                    set_preview_error(library, "FILE");
                    None
                }
            },
            Err(err) => {
                esp_println::println!("epub: open /books failed: {:?}", err);
                set_preview_error(library, "BOOKS DIR");
                None
            }
        };
        status_for_load_result(load_result, library)
    } else {
        let load_result = match root.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
            Ok(file) => Some(build_or_load_epub_cache_from_file(
                file,
                root,
                &display_name,
                requested_chapter,
                target_pages,
                library,
                scratch,
                font_metrics,
            )),
            Err(err) => {
                esp_println::println!("epub: open file failed: {:?}", err);
                set_preview_error(library, "FILE");
                None
            }
        };
        status_for_load_result(load_result, library)
    };
    if matches!(status, BookLoadStatus::Ready) && !library.title.is_empty() {
        // Persist the just-learned EPUB title into the catalog record, in
        // this same session, so future Library windows and boots label the
        // book without probing its cache. Read-compare first; a reopen with
        // an unchanged title writes nothing.
        let _ = crate::library_sd::update_catalog_title(
            root,
            index,
            source_identity,
            library.title.as_str(),
        );
    }
    status
}

#[inline(never)]
pub(crate) fn store_app_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    record: AppStateRecord,
) -> bool {
    // The same session lands the global record and, for SD books, the
    // per-book position beside that book's cache, so switching books
    // never abandons the previous one's place.
    let book_key = app_core::ReaderSource::from_book_id(record.book_id)
        .sd_index()
        .and_then(|index| library.catalog_entry(index as usize))
        .map(|entry| proto::cache::cache_key_for(entry.display_name.as_str(), entry.byte_size));
    sd_session::with_root(epd, sd_cs, |root| {
        let state = reader_cache_files::write_state_file(root, record);
        let position = if let Some(key) = &book_key {
            reader_cache_files::write_position_file(
                root,
                key.as_str(),
                record.chapter,
                record.screen,
            )
        } else {
            Ok(())
        };
        state.and(position)
    })
    .ok()
    .is_some_and(|result| result.is_ok())
}

/// The saved per-book position for a catalog entry, if any.
#[inline(never)]
pub(crate) fn load_position(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: usize,
) -> Option<(u16, u32)> {
    let key = library
        .catalog_entry(index)
        .map(|entry| proto::cache::cache_key_for(entry.display_name.as_str(), entry.byte_size))?;
    sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::read_position_file(root, key.as_str())
    })
    .ok()
    .flatten()
}

/// Load the book's full chapter list from TOC.BIN into the reader's section
/// buffer for the Chapters overview. The reading section reloads on exit.
#[inline(never)]
pub(crate) fn load_chapters_into_store(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    selection: usize,
) -> bool {
    let Some(entry) = library.catalog_entry(index) else {
        return false;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let key = proto::cache::cache_key_for(entry.display_name.as_str(), source_identity.1);
    // Center the window on the selection so scrolling either way has slack
    // before the next reload.
    let window_start = selection.saturating_sub(crate::reader_store::TOC_WINDOW_CAPACITY / 2);
    sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::load_v2_toc_into_text(
            root,
            key.as_str(),
            source_identity,
            library,
            window_start,
        )
    })
    .unwrap_or(false)
}

/// Make the TOC window cover the Chapters rows visible around `selection`,
/// reloading it from TOC.BIN only on a miss — the overview analogue of
/// `ensure_library_window`. Cheap when the window already covers.
pub(crate) fn ensure_toc_window(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    selection: usize,
    portrait: bool,
) -> bool {
    let first_visible =
        ui::render::toc_scroll_start(selection, library.overview_chapter_count(), portrait);
    if library.toc_window_covers(first_visible, ui::render::toc_visible_rows(portrait)) {
        return true;
    }
    load_chapters_into_store(epd, sd_cs, library, index, selection)
}

#[inline(never)]
pub(crate) fn store_wifi_credentials(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    record: hal_ext::nvm::WifiCredentialsRecord,
) -> bool {
    sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::write_wifi_file(root, record).is_ok()
    })
    .unwrap_or(false)
}

#[inline(never)]
pub(crate) fn forget_wifi_credentials(epd: &mut Epd, sd_cs: &mut Output<'static>) -> bool {
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::delete_wifi_file(root)
    })
    .unwrap_or(false)
}

#[inline(never)]
pub(crate) fn load_wifi_credentials(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
) -> Option<hal_ext::nvm::WifiCredentialsRecord> {
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| reader_cache_files::read_wifi_file(root))
        .ok()
        .flatten()
}

/// Kept out of line for the same stack discipline as the store side.
#[inline(never)]
pub(crate) fn load_app_state(epd: &mut Epd, sd_cs: &mut Output<'static>) -> Option<AppStateRecord> {
    // Not point-free: the generic fn item fails the closure's HRTB check.
    #[allow(clippy::redundant_closure)]
    sd_session::with_root(epd, sd_cs, |root| reader_cache_files::read_state_file(root))
        .ok()
        .flatten()
}

#[inline(never)]
pub(crate) fn load_custom_font_manifest(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
) {
    if display::font::builtin_custom_available() {
        library.set_custom_font(
            Some(display::font::builtin_custom_name()),
            display::font::builtin_custom_identity(),
            &[],
        );
        esp_println::println!(
            "font: builtin custom '{}' identity={:016x}",
            display::font::builtin_custom_name(),
            display::font::builtin_custom_identity()
        );
        return;
    }
    let manifest = sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::read_custom_font_manifest(root)
    })
    .ok()
    .flatten();
    if let Some(manifest) = manifest {
        esp_println::println!(
            "font: custom '{}' identity={:016x}",
            manifest.name.as_str(),
            manifest.identity
        );
        library.set_custom_font(
            Some(manifest.name.as_str()),
            manifest.identity,
            &manifest.faces[..manifest.face_count],
        );
    } else {
        esp_println::println!("font: no custom font pack");
        library.set_custom_font(None, 0, &[]);
    }
}

/// Track the chapter for the page just rendered while reading, past the
/// reducer's 128-chapter `sd_chapter_for_page` cap. Cheap in-RAM resolve every
/// render; only touches SD (a 48-byte TOC title read) when the chapter actually
/// changes. Returns the new uncapped chapter so the caller can forward it to the
/// reducer, else `None` when nothing changed (or the page map is not resident,
/// e.g. a built-in book).
#[inline(never)]
pub(crate) fn track_reading_chapter(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    global_page: u32,
    library: &mut ReaderStore,
) -> Option<u16> {
    if library.chapter_page_count == 0 {
        return None;
    }
    let current = library.current_chapter_for_page(global_page);
    if current == library.current_chapter() {
        return None;
    }
    let index = library.loaded_index?;
    load_chapter_title(epd, sd_cs, index, current, library);
    Some(current)
}

/// Read a chapter's title from the book's TOC.BIN and make it the resident
/// current chapter. Used at boot restore (so wake-to-Home names the chapter
/// before the book is opened) and on reading renders when the chapter changes.
/// Tags the title with the book's source identity; a colophon shows it only for
/// that book.
#[inline(never)]
pub(crate) fn load_chapter_title(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
    chapter: u16,
    library: &mut ReaderStore,
) {
    let Some(entry) = library.catalog_entry(index) else {
        return;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(&entry.display_name);
    let cache_key = proto::cache::cache_key_for(display_name.as_str(), source_identity.1);
    let found = sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::read_v2_toc_chapter_title(
            root,
            cache_key.as_str(),
            source_identity,
            chapter,
            library,
        )
    })
    .unwrap_or(false);
    if !found {
        // Tag the source even on a miss so a stale title from another book is
        // never shown; the colophon falls back to a numeral for this chapter.
        library.set_current_chapter(chapter, "", source_identity);
    }
}

/// Read the book's total page count from its V2 index header at boot restore so
/// the Home progress bar has a denominator before the book is opened. Returns 0
/// if unavailable (caller leaves the bar on its fallback).
#[inline(never)]
pub(crate) fn restore_book_page_count(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
    library: &ReaderStore,
) -> u32 {
    let Some(entry) = library.catalog_entry(index) else {
        return 0;
    };
    let source_identity = (entry.source_hash, entry.byte_size);
    let mut display_name = String::<64>::new();
    let _ = display_name.push_str(&entry.display_name);
    let cache_key = proto::cache::cache_key_for(display_name.as_str(), source_identity.1);
    sd_session::with_root(epd, sd_cs, |root| {
        reader_cache_files::read_v2_book_total_pages(
            root,
            cache_key.as_str(),
            source_identity,
            library,
        )
    })
    .unwrap_or(0)
}

#[inline(never)]
fn try_load_v2_book_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &str,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    started: Instant,
    label: &str,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match reader_cache_files::load_v2_book_index(root, cache_key, source_identity, library) {
        BookIndexLoadResult::Hit => {
            match reader_cache_files::load_v2_section_by_global_page(
                root,
                cache_key,
                source_identity,
                requested_global_page,
                library,
            ) {
                CacheLoadResult::Hit { pages, .. } => {
                    reader_layout::rebuild_toc_page_targets(library);
                    refresh_chapter_tracking(
                        root,
                        cache_key,
                        source_identity,
                        requested_global_page,
                        library,
                    );
                    let cover = reader_cache_files::load_v2_cover_cache(root, cache_key, library);
                    esp_println::println!(
                        "epub: v2 {label} book cache ready after {} ms (total={} section_pages={} toc={} cover={:?})",
                        started.elapsed().as_millis(),
                        library.advertised_page_count(),
                        pages,
                        library.toc_count(),
                        cover
                    );
                    true
                }
                other => {
                    esp_println::println!("epub: {label} book index section load {:?}", other);
                    false
                }
            }
        }
        BookIndexLoadResult::Invalid => {
            esp_println::println!("epub: v2 {label} book index invalid");
            false
        }
        BookIndexLoadResult::Miss => {
            esp_println::println!("epub: v2 {label} book index miss");
            false
        }
    }
}

/// Keep the resident current-chapter index and title pointed at the page just
/// loaded. The full chapter-page map is read from TOC.BIN once per book (or
/// after a repaginating settings change); the title is a single 48-byte record
/// re-read only when the chapter actually changes.
fn refresh_chapter_tracking<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &str,
    source_identity: (u32, u32),
    global_page: u32,
    library: &mut ReaderStore,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let config = reader_layout::reader_layout_config(library.type_settings(), library.portrait());
    let token = (
        source_identity.0,
        source_identity.1,
        config,
        library.custom_font_identity(),
    );
    if library.chapter_page_count == 0 || library.chapter_page_token != token {
        if reader_cache_files::load_v2_toc_page_map(root, cache_key, source_identity, library) {
            library.chapter_page_token = token;
        } else {
            return;
        }
    }
    let current = library.current_chapter_for_page(global_page);
    let needs_refresh =
        current != library.current_chapter() || library.current_chapter_title().is_empty();
    if needs_refresh
        && !reader_cache_files::read_v2_toc_chapter_title(
            root,
            cache_key,
            source_identity,
            current,
            library,
        )
    {
        // No title on the card (or a short read): still advance the index so
        // the cursor tracks; the colophon falls back to a numeral.
        library.set_current_chapter(current, "", source_identity);
    }
}

fn set_preview_error(library: &mut ReaderStore, message: &str) {
    library.set_reader_error(message);
}

fn status_for_load_result(
    result: Option<Result<(), ReaderCacheError>>,
    library: &mut ReaderStore,
) -> BookLoadStatus {
    match result {
        Some(Ok(())) => BookLoadStatus::Ready,
        Some(Err(err)) => {
            esp_println::println!("epub: load failed: {:?}", err);
            set_preview_error_from_error(library, err);
            BookLoadStatus::Error
        }
        None => BookLoadStatus::Error,
    }
}

fn session_error_label(error: SdSessionError) -> &'static str {
    match error {
        SdSessionError::CardInit => "CARD INIT",
        SdSessionError::Volume => "VOLUME",
        SdSessionError::Root => "ROOT",
    }
}

fn set_preview_error_from_error(library: &mut ReaderStore, error: ReaderCacheError) {
    let message = match error {
        ReaderCacheError::Zip(proto::epub::ZipError::OutputTooSmall) => "EPUB TOO BIG",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryBufferTooSmall) => "PATH LONG",
        ReaderCacheError::Zip(proto::epub::ZipError::UnsupportedCompression) => "ZIP METHOD",
        ReaderCacheError::Zip(proto::epub::ZipError::EntryNotFound) => "ZIP MISSING",
        ReaderCacheError::Zip(proto::epub::ZipError::Inflate) => "ZIP INFLATE",
        ReaderCacheError::Zip(proto::epub::ZipError::Io) => "OPEN BUDGET",
        ReaderCacheError::Zip(_) => "ZIP",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManyManifestItems) => "OPF MANIFEST",
        ReaderCacheError::Epub(proto::epub::EpubError::TooManySpineItems) => "OPF SPINE",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpfPath) => "NO OPF",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpf) => "NO OPF FILE",
        ReaderCacheError::Epub(proto::epub::EpubError::Utf8) => "OPF UTF8",
        ReaderCacheError::Epub(proto::epub::EpubError::Zip(_)) => "OPF ZIP",
        ReaderCacheError::Epub(_) => "OPF",
        ReaderCacheError::Xhtml(proto::epub::XhtmlError::TooManyRuns) => "TEXT FULL",
        ReaderCacheError::Utf8 => "UTF8",
        ReaderCacheError::MissingSpine => "NO SPINE",
        ReaderCacheError::NoBodyText => "NO BODY TEXT",
        ReaderCacheError::SectionRead => "SECTION READ",
        ReaderCacheError::IndexWrite => "CACHE WRITE",
        ReaderCacheError::EntryNameTooLong => "PATH LONG",
    };
    set_preview_error(library, message);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderCacheError {
    Zip(proto::epub::ZipError),
    Epub(proto::epub::EpubError),
    Xhtml(proto::epub::XhtmlError),
    Utf8,
    MissingSpine,
    NoBodyText,
    /// The book built and its cache was written, but the requested section
    /// failed to load back from the card.
    SectionRead,
    /// The book built, but writing BOOK.BIN failed partway; the cache dir
    /// was cleared because a truncated index serves no load path.
    IndexWrite,
    EntryNameTooLong,
}

impl From<proto::epub::ZipError> for ReaderCacheError {
    fn from(value: proto::epub::ZipError) -> Self {
        Self::Zip(value)
    }
}

impl From<proto::epub::EpubError> for ReaderCacheError {
    fn from(value: proto::epub::EpubError) -> Self {
        Self::Epub(value)
    }
}

impl From<proto::epub::XhtmlError> for ReaderCacheError {
    fn from(value: proto::epub::XhtmlError) -> Self {
        Self::Xhtml(value)
    }
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_or_load_epub_cache_from_file<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_path: &str,
    _requested_chapter: u8,
    target_pages: usize,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> Result<(), ReaderCacheError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let open_started = Instant::now();
    let source_len = file.length();
    let source_identity = (source_hash(source_path, source_len), source_len);
    let cache_key = proto::cache::cache_key_for(source_path, source_len);
    library.set_cache_key(cache_key.as_str());

    esp_println::println!("epub: stage OpenSdFile len={}", source_len);
    let requested_global_page = target_pages as u32;

    esp_println::println!("epub: zip open len={}", source_len);
    let reader = SdFileReadAt {
        file,
        len: source_len,
        read_ops: 0,
        read_bytes: 0,
    };
    let zip = ZipStream::new(reader, scratch.tail)?;
    esp_println::println!(
        "epub: zip ready after {} ms",
        open_started.elapsed().as_millis()
    );

    build_or_load_epub_cache_from_zip(
        zip,
        root,
        source_path,
        source_identity,
        cache_key.as_str(),
        requested_global_page,
        open_started,
        library,
        font_metrics,
        ZipBuildScratch {
            header: scratch.header,
            name: scratch.name,
            compressed: scratch.compressed,
            container: scratch.container,
            opf: scratch.opf,
            xhtml: scratch.xhtml,
            book_sections: scratch.book_sections,
            zip_inflate: &mut scratch.zip_inflate,
        },
    )
}

struct ZipBuildScratch<'a> {
    header: &'a mut [u8; READER_HEADER_SCRATCH],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    container: &'a mut [u8; READER_CONTAINER_SCRATCH],
    opf: &'a mut [u8; READER_OPF_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    book_sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    zip_inflate: &'a mut ZipInflateScratch,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_or_load_epub_cache_from_zip<
    Z,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    mut zip: Z,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_path: &str,
    source_identity: (u32, u32),
    cache_key: &str,
    requested_global_page: u32,
    open_started: Instant,
    library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    scratch: ZipBuildScratch<'_>,
) -> Result<(), ReaderCacheError>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let io_start = crate::sd_session::sd_stats::snapshot();
    let mut section_write_micros: u64 = 0;
    esp_println::println!("epub: stage ParseContainerAndOpf");
    // ~260 B stack local: copying the OPF path out releases the borrow on
    // scratch.container so the capture stage below can reuse that buffer
    // during the spine walk. Memory delta: +260 B in the EPUB-open frame;
    // both boards' release links (stack ASSERT) pass with it.
    let mut opf_path_buf = String::<256>::new();
    proto::epub::load_container_xml_and_find_opf_path(
        &mut zip,
        scratch.header,
        scratch.name,
        scratch.compressed,
        scratch.container,
        &mut *scratch.zip_inflate,
        &mut opf_path_buf,
    )
    .map_err(|err| match err {
        // Keep the pre-helper diagnostics: zip failures surface as zip
        // errors and container text failures as "UTF8", never as
        // OPF-stage labels.
        proto::epub::EpubError::Zip(zip_err) => ReaderCacheError::Zip(zip_err),
        proto::epub::EpubError::Utf8 => ReaderCacheError::Utf8,
        other => ReaderCacheError::Epub(other),
    })?;
    let opf_path = opf_path_buf.as_str();

    let opf_entry = zip.find_entry(opf_path, scratch.header, scratch.name)?;
    esp_println::println!(
        "epub: opf compressed={} uncompressed={}",
        opf_entry.compressed_size,
        opf_entry.uncompressed_size
    );
    let (opf_len, opf_complete) = zip.read_entry_prefix_streamed(
        opf_entry,
        scratch.compressed,
        scratch.opf,
        &mut *scratch.zip_inflate,
    )?;
    if !opf_complete {
        esp_println::println!(
            "epub: opf prefix truncated at {} of {} bytes",
            opf_len,
            opf_entry.uncompressed_size
        );
    }
    let opf_xml =
        core::str::from_utf8(&scratch.opf[..opf_len]).map_err(|_| ReaderCacheError::Utf8)?;
    let package = parse_opf(opf_xml, BookId(2), source_path, 0, opf_path)?;
    esp_println::println!(
        "epub: opf parsed after {} ms (spine={} truncated={})",
        open_started.elapsed().as_millis(),
        package.spine.len(),
        package.spine_truncated
    );

    library.set_book_labels(package.meta.title, package.meta.author);
    library.clear_cover();
    if zip.is_forward_only() {
        library.clear_toc();
    } else {
        // Stream the whole chapter list into the (currently idle) xhtml
        // scratch as fixed records, then write it to TOC.BIN so the overview
        // can read it from the card instead of holding it all resident.
        let toc_record_count = load_epub_toc(
            &mut zip,
            opf_path,
            &package,
            library,
            &mut scratch.xhtml[..],
            TocScratch {
                header: scratch.header,
                name: scratch.name,
                compressed: scratch.compressed,
                zip_inflate: &mut *scratch.zip_inflate,
            },
        );
        let toc_bytes = toc_record_count
            .saturating_mul(proto::cache::TOC_CHAPTER_RECORD_BYTES)
            .min(scratch.xhtml.len());
        let wrote_toc = reader_cache_files::write_v2_toc_file(
            root,
            cache_key,
            source_identity,
            toc_record_count,
            &scratch.xhtml[..toc_bytes],
        );
        esp_println::println!(
            "epub: toc.bin wrote {} chapter(s) ok={}",
            toc_record_count,
            wrote_toc
        );
    }
    esp_println::println!(
        "epub: toc parsed after {} ms ({} item(s))",
        open_started.elapsed().as_millis(),
        library.toc_count
    );
    let css_rules = CssRules::new();

    esp_println::println!("epub: stage BuildV2BookCache");
    let spine_started = Instant::now();
    let mut xhtml_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    scratch.book_sections.fill(EMPTY_BOOK_SECTION_RECORD);
    let sections = &mut *scratch.book_sections;
    let mut section_count = 0usize;
    let mut total_pages = 0u32;
    let mut saw_spine = false;
    // A spine clipped at MAX_SPINE_ITEMS means the tail chapters were dropped at
    // parse, so the book is partial even if every kept section caches cleanly.
    let mut book_partial = package.spine_truncated;
    let visible_page_capacity = library.page_capacity().max(1);
    let generate_toc_from_headings = library.toc_count() == 0;
    let start_spine_index = package
        .text_reference_href
        .and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| href_matches_spine(href, item.href.of(package.opf_text)))
        })
        .unwrap_or_else(|| inferred_start_spine_index(&package));

    // The whole cache tree is created here, once; the capture and the
    // sections walk below only open what already exists.
    let cache_dirs_ok = reader_cache_files::ensure_v2_cache_dirs(root, cache_key).is_ok();
    if !cache_dirs_ok {
        esp_println::println!("cache: v2 ensure dirs failed key={}", cache_key);
    }
    // Capture the push_block stream into CONT.BIN alongside the build, so a
    // type-settings change can replay it without re-parsing the EPUB. The
    // capture is a pure accelerator: it disables itself on any failure and
    // never fails the build. Its directory handle outlives the whole walk
    // because the capture's file handle borrows it.
    let content_dir = if cache_dirs_ok {
        reader_cache_files::open_v2_book_dir(root, cache_key)
    } else {
        None
    };
    // One SD block of capture staging, carved from the 4 KB container
    // scratch — container.xml parsing is finished with it by now, so this
    // costs no new RAM. The FAT layer pays per-write-call overhead (block
    // lookup plus read-modify-write of partial sectors), so a full-block
    // stage batches twice the records per call compared to the earlier
    // 256 B stage.
    const CONTENT_STAGE_BYTES: usize = 512;
    let (stage_buf, _) = scratch.container.split_at_mut(CONTENT_STAGE_BYTES);
    let mut content =
        reader_cache_files::ContentCapture::begin(content_dir.as_ref(), source_identity, stage_buf);

    // The SECTIONS directory stays open for the whole spine walk; every
    // section flush is one file open instead of a four-level dir walk.
    let walk = reader_cache_files::with_v2_sections_dir(root, cache_key, |sections_dir| {
        for (spine_index, spine) in package.spine.iter().enumerate().filter(|(index, item)| {
            *index >= start_spine_index
                && !item.href.is_empty()
                && !spine_item_is_navigation(item, &package)
        }) {
            if section_count >= sections.len() {
                book_partial = true;
                break;
            }
            saw_spine = true;
            resolve_epub_href(opf_path, spine.href.of(package.opf_text), &mut xhtml_path)?;
            esp_println::println!("epub: find spine {}", xhtml_path.as_str());
            let Ok(xhtml_entry) = zip.find_entry(&xhtml_path, scratch.header, scratch.name) else {
                continue;
            };
            esp_println::println!(
                "epub: spine {} compressed={} uncompressed={}",
                xhtml_path.as_str(),
                xhtml_entry.compressed_size,
                xhtml_entry.uncompressed_size
            );
            let spine_u16 = spine_index.min(u16::MAX as usize) as u16;
            let mut sink = LibraryBlockSink::begin_spine(
                &mut *library,
                root,
                sections_dir,
                source_identity,
                &mut *font_metrics,
                &mut *sections,
                &mut section_count,
                &mut total_pages,
                &mut book_partial,
                &mut section_write_micros,
                spine_u16,
                visible_page_capacity,
                generate_toc_from_headings,
            );
            // Stream the whole member through the resumable block parser in
            // bounded windows: spine XHTML of any size decodes completely, with
            // no 24 KB prefix truncation. The parser's in-body assumption is
            // sniffed from the first decoded window, mirroring the
            // whole-document contains() check.
            let mut capture_sink = CapturingBlockSink {
                inner: &mut sink,
                capture: &mut content,
                spine_index: spine_u16,
            };
            let mut tokenizer = StreamingXmlTokenizer::new();
            let mut parser: Option<XhtmlBlockStreamParser> = None;
            let mut parse_error: Option<XhtmlError> = None;
            let read_result = zip.read_entry_to_sink(
                xhtml_entry,
                scratch.compressed,
                scratch.xhtml,
                &mut *scratch.zip_inflate,
                &mut |chunk| {
                    let parser = parser.get_or_insert_with(|| {
                        let has_body =
                            bytes_contain(chunk, b"<body") || bytes_contain(chunk, b":body");
                        XhtmlBlockStreamParser::new(!has_body)
                    });
                    tokenizer
                        .feed_xhtml_blocks(chunk, parser, Some(&css_rules), &mut capture_sink)
                        .map_err(|err| {
                            parse_error = Some(err);
                            proto::epub::ZipError::Inflate
                        })
                },
            );
            match read_result {
                Ok(()) => {
                    if let Some(parser) = parser.as_mut() {
                        if let Err(err) = tokenizer.finish_xhtml_blocks(parser, &mut capture_sink) {
                            if !capture_sink.inner.stopped {
                                return Err(err.into());
                            }
                        }
                    }
                }
                Err(_) if parse_error.is_some() => {
                    let err = parse_error.take().expect("parse error recorded");
                    if capture_sink.inner.stopped {
                        esp_println::println!(
                            "epub: bounded open stopped at spine {} after {} section(s): {:?}",
                            spine_index,
                            *capture_sink.inner.section_count,
                            err
                        );
                    } else {
                        return Err(err.into());
                    }
                }
                Err(err) => return Err(err.into()),
            }
            sink.finish_spine(false);
            content.spine_end(spine_u16);
        }
        Ok::<(), ReaderCacheError>(())
    });

    // Keep CONT.BIN only when the walk captured the whole book: a partial
    // or failed capture is deleted so replay can never resurrect a
    // truncated stream.
    let content_kept = content.finish(
        content_dir.as_ref(),
        walk.is_ok() && !book_partial && section_count > 0 && total_pages > 0,
    );
    esp_println::println!("epub: content capture kept={}", content_kept);
    walk?;

    if section_count > 0 && total_pages > 0 {
        let published = publish_book_cache(
            root,
            cache_key,
            source_identity,
            requested_global_page,
            "full",
            open_started,
            spine_started,
            io_start,
            section_write_micros,
            library,
            &sections[..section_count],
            total_pages,
            book_partial,
        );
        match published {
            BookPublishOutcome::Ready => Ok(()),
            BookPublishOutcome::SectionReadFailed => {
                // The build and index write succeeded; only the requested
                // section failed to read back. Keep the freshly written
                // cache — the next open retries the fast path against it —
                // and report the read failure rather than a bogus content
                // diagnosis.
                Err(ReaderCacheError::SectionRead)
            }
            BookPublishOutcome::IndexWriteFailed => {
                // The index write failed partway, so BOOK.BIN may be a
                // truncated file that serves neither the fast path nor
                // replay (the labels load bails on it). Clear the debris —
                // sections and CONT.BIN are useless without an index — so
                // the next open rebuilds from the EPUB cleanly.
                reader_cache_files::empty_cache_dir(root, cache_key, section_count as u16);
                Err(ReaderCacheError::IndexWrite)
            }
        }
    } else if saw_spine {
        Err(ReaderCacheError::NoBodyText)
    } else {
        Err(ReaderCacheError::MissingSpine)
    }
}

/// How the publish tail ended. The two failures need different cleanup:
/// a failed index write leaves a truncated BOOK.BIN behind (created or
/// truncated before the body writes), while a section read-back miss
/// leaves a fully written, valid cache worth keeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BookPublishOutcome {
    Ready,
    IndexWriteFailed,
    SectionReadFailed,
}

/// The shared publish tail of a section-cache build: write BOOK.BIN, load
/// the requested section, refresh chapter tracking and the cover, and print
/// the ready/bench lines. Used by the full EPUB build (`label` "full") and
/// the CONT.BIN replay path (`label` "replay").
#[expect(clippy::too_many_arguments)] // Intentionally retained wide signature to pass borrow-checked references and telemetry stats directly without struct wrapping
fn publish_book_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &str,
    source_identity: (u32, u32),
    requested_global_page: u32,
    label: &str,
    open_started: Instant,
    spine_started: Instant,
    io_start: crate::sd_session::sd_stats::Snapshot,
    section_write_micros: u64,
    library: &mut ReaderStore,
    sections_slice: &[BookV2SectionRecord],
    total_pages: u32,
    book_partial: bool,
) -> BookPublishOutcome
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let wrote_index = reader_cache_files::write_v2_book_index(
        root,
        cache_key,
        source_identity,
        total_pages,
        sections_slice,
        library,
        book_partial,
    );
    if !wrote_index {
        return BookPublishOutcome::IndexWriteFailed;
    }
    library.set_book_index(total_pages, book_partial, sections_slice);
    let hit = matches!(
        reader_cache_files::load_v2_section_by_global_page(
            root,
            cache_key,
            source_identity,
            requested_global_page.min(total_pages.saturating_sub(1)),
            library,
        ),
        CacheLoadResult::Hit { .. }
    );
    if !hit {
        return BookPublishOutcome::SectionReadFailed;
    }
    reader_layout::rebuild_toc_page_targets(library);
    refresh_chapter_tracking(
        root,
        cache_key,
        source_identity,
        requested_global_page.min(total_pages.saturating_sub(1)),
        library,
    );
    let cover = reader_cache_files::load_v2_cover_cache(root, cache_key, library);
    esp_println::println!("epub: stage PublishLoaded");
    esp_println::println!(
        "epub: {label} book cache ready after {} ms (total={} sections={} partial={} cover={:?} key {})",
        open_started.elapsed().as_millis(),
        total_pages,
        sections_slice.len(),
        book_partial,
        cover,
        cache_key
    );
    let io = crate::sd_session::sd_stats::snapshot().since(io_start);
    esp_println::println!(
        "bench: storage_build elapsed_ms={} spine_ms={} write_ms={} sections={} pages={} rd_calls={} rd_blocks={} wr_calls={} wr_blocks={} key={}",
        open_started.elapsed().as_millis(),
        spine_started.elapsed().as_millis(),
        section_write_micros / 1000,
        sections_slice.len(),
        total_pages,
        io.read_calls,
        io.read_blocks,
        io.write_calls,
        io.write_blocks,
        cache_key
    );
    BookPublishOutcome::Ready
}

/// Forwards `push_block` to the build sink while recording the exact call
/// into the content capture. Wrapping at the trait seam keeps the capture
/// out of `LibraryBlockSink`'s already-wide borrow set, and guarantees the
/// captured stream is byte-for-byte what the sink consumed.
struct CapturingBlockSink<
    'w,
    'v,
    's,
    S,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    S: XhtmlBlockSink,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    inner: &'w mut S,
    capture:
        &'w mut reader_cache_files::ContentCapture<'v, 's, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    spine_index: u16,
}

impl<S, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    XhtmlBlockSink for CapturingBlockSink<'_, '_, '_, S, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    S: XhtmlBlockSink,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError> {
        self.capture
            .push_block_record(self.spine_index, text, role, style, align, paragraph_end);
        self.inner
            .push_block(text, role, style, align, paragraph_end)
    }
}

/// Rebuild the section cache by replaying CONT.BIN — the captured
/// `push_block` stream — through the same build sink, skipping the zip
/// read, inflate, and XML parse entirely. Runs when the v2 index missed
/// (typically a type-settings change); returns true when the book
/// published Ready. Any validation or framing failure deletes the file and
/// returns false, so the caller falls back to the full EPUB build.
#[inline(never)]
fn try_replay_content_cache<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &str,
    source_identity: (u32, u32),
    requested_global_page: u32,
    library: &mut ReaderStore,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Why a replay attempt ended without publishing.
    enum ReplayFail {
        /// No usable attempt was possible (no file, or the old index's
        /// labels/TOC couldn't be recovered) — leave CONT.BIN alone.
        Bail,
        /// CONT.BIN exists but is stale, truncated, or corrupt: delete it.
        Corrupt,
    }

    let open_started = Instant::now();
    let io_start = crate::sd_session::sd_stats::snapshot();
    let spine_started = Instant::now();
    scratch.book_sections.fill(EMPTY_BOOK_SECTION_RECORD);
    let sections = &mut *scratch.book_sections;
    let xhtml_buf = &mut scratch.xhtml[..];
    let mut section_count = 0usize;
    let mut total_pages = 0u32;
    let mut book_partial = false;
    let mut section_write_micros: u64 = 0;

    let replayed = reader_cache_files::with_v2_sections_dir(root, cache_key, |sections_dir| {
        // One open serves both the header validation and the record
        // stream; the handle stays live for the whole replay.
        reader_cache_files::with_v2_content_file(root, cache_key, Mode::ReadOnly, |file| {
            let mut header_bytes = [0u8; CONTENT_HEADER_BYTES];
            if reader_cache_files::read_exact_file(file, &mut header_bytes).is_err() {
                return Err(ReplayFail::Corrupt);
            }
            let Ok(header) = proto::cache::decode_content_header(&header_bytes) else {
                return Err(ReplayFail::Corrupt);
            };
            if !header.complete
                || header.source_hash != source_identity.0
                || header.source_size != source_identity.1
            {
                // Incomplete or written for a different file under the
                // same key: dead weight.
                return Err(ReplayFail::Corrupt);
            }
            // The old BOOK.BIN is layout-invalid but its labels and TOC
            // copy are settings-independent; carry them into the rewritten
            // index. Bail to the full build when they can't be recovered —
            // it re-parses them.
            if !reader_cache_files::load_v2_book_labels_and_toc(
                root,
                cache_key,
                source_identity,
                library,
            ) {
                return Err(ReplayFail::Bail);
            }
            library.clear_cover();
            esp_println::println!("epub: stage ReplayContentCache");
            let visible_page_capacity = library.page_capacity().max(1);
            let generate_toc_from_headings = library.toc_count() == 0;
            replay_content_records(
                file,
                root,
                sections_dir,
                source_identity,
                library,
                font_metrics,
                sections,
                &mut section_count,
                &mut total_pages,
                &mut book_partial,
                &mut section_write_micros,
                xhtml_buf,
                visible_page_capacity,
                generate_toc_from_headings,
                header.spine_count,
                header.content_len,
            )
            .map_err(|()| ReplayFail::Corrupt)
        })
        .unwrap_or(Err(ReplayFail::Bail))
    });

    match replayed {
        Err(ReplayFail::Bail) => return false,
        Err(ReplayFail::Corrupt) => {
            esp_println::println!("epub: content replay failed, falling back to full build");
            reader_cache_files::delete_v2_content_file(root, cache_key);
            return false;
        }
        Ok(()) if section_count == 0 || total_pages == 0 => {
            esp_println::println!("epub: content replay failed, falling back to full build");
            reader_cache_files::delete_v2_content_file(root, cache_key);
            return false;
        }
        Ok(()) => {}
    }
    let published = publish_book_cache(
        root,
        cache_key,
        source_identity,
        requested_global_page,
        "replay",
        open_started,
        spine_started,
        io_start,
        section_write_micros,
        library,
        &sections[..section_count],
        total_pages,
        book_partial,
    );
    if published != BookPublishOutcome::Ready {
        // Either failure leaves the replay's cache state unusable (a
        // truncated BOOK.BIN or an unreadable section); the full build
        // rewrites everything, so clear it all either way.
        esp_println::println!("epub: content replay publishing failed, falling back to full build");
        reader_cache_files::empty_cache_dir(root, cache_key, section_count as u16);
        return false;
    }
    true
}

/// Stream CONT.BIN's records into fresh `LibraryBlockSink` runs, one per
/// captured spine item, mirroring the full build's spine loop. The framing
/// discipline — marker-terminated groups, no mid-group spine change,
/// clean EOF only after every expected spine — is owned and enforced by
/// `proto::cache::replay_content_stream`, which the host regression tests
/// drive directly; this function only supplies the file reader and the
/// per-group sink runs.
#[expect(clippy::too_many_arguments)] // Intentionally retained wide signature to pass borrow-checked references and layout settings directly without struct wrapping
fn replay_content_records<
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    file: &File<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    source_identity: (u32, u32),
    library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    sections: &mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    section_count: &mut usize,
    total_pages: &mut u32,
    book_partial: &mut bool,
    write_micros: &mut u64,
    buf: &mut [u8],
    visible_page_capacity: usize,
    generate_toc_from_headings: bool,
    expected_spines: u16,
    expected_len: u32,
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if file.length() != expected_len {
        return Err(());
    }
    if file.seek_from_start(CONTENT_HEADER_BYTES as u32).is_err() {
        return Err(());
    }
    let mut read = |dst: &mut [u8]| file.read(dst).map_err(|_| proto::cache::ContentReplayError);
    let mut on_group =
        |group_spine: u16,
         group: &mut proto::cache::ContentGroupReader<'_>|
         -> Result<proto::cache::ContentReplayFlow, proto::cache::ContentReplayError> {
            if *section_count >= sections.len() {
                *book_partial = true;
                return Ok(proto::cache::ContentReplayFlow::Stop);
            }
            let mut sink = LibraryBlockSink::begin_spine(
                &mut *library,
                root,
                sections_dir,
                source_identity,
                &mut *font_metrics,
                &mut *sections,
                &mut *section_count,
                &mut *total_pages,
                &mut *book_partial,
                &mut *write_micros,
                group_spine,
                visible_page_capacity,
                generate_toc_from_headings,
            );
            while let Some((record, text)) = group.next_block()? {
                if sink
                    .push_block(
                        text,
                        record.role,
                        record.style,
                        record.align,
                        record.paragraph_end,
                    )
                    .is_err()
                {
                    if sink.stopped {
                        // Section capacity exhausted at these settings — the
                        // same stop a full build takes; publish partial.
                        sink.finish_spine(false);
                        return Ok(proto::cache::ContentReplayFlow::Stop);
                    }
                    return Err(proto::cache::ContentReplayError);
                }
            }
            sink.finish_spine(false);
            Ok(proto::cache::ContentReplayFlow::Continue)
        };
    proto::cache::replay_content_stream(&mut read, buf, expected_spines, &mut on_group)
        .map(|_| ())
        .map_err(|_| ())
}

fn spine_item_is_navigation(
    item: &proto::epub::SpineItem,
    package: &proto::epub::EpubPackage<'_>,
) -> bool {
    let opf = package.opf_text;
    let href = item.href.of(opf);
    let lower_href = LowerAscii::<160>::new(href);
    let lower_props = LowerAscii::<96>::new(item.properties.of(opf));
    item.media_type.of(opf) == "application/x-dtbncx+xml"
        || package.nav_href.map(|nav| nav == href).unwrap_or(false)
        || package.ncx_href.map(|ncx| ncx == href).unwrap_or(false)
        || lower_props.word_eq("nav")
        || lower_href.ends_with("toc.xhtml")
        || lower_href.ends_with("toc.html")
        || lower_href.ends_with("nav.xhtml")
        || lower_href.ends_with("nav.html")
}

fn inferred_start_spine_index(package: &proto::epub::EpubPackage<'_>) -> usize {
    if package.spine.len() <= 1 {
        return 0;
    }
    let Some(first) = package.spine.first() else {
        return 0;
    };
    let lower_href = LowerAscii::<MAX_ENTRY_NAME_BYTES>::new(first.href.of(package.opf_text));
    if lower_href.contains("titlepage")
        || lower_href.contains("title-page")
        || lower_href.contains("cover")
    {
        1
    } else {
        0
    }
}

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn load_epub_toc<Z>(
    zip: &mut Z,
    opf_path: &str,
    package: &proto::epub::EpubPackage<'_>,
    library: &mut ReaderStore,
    toc_buf: &mut [u8],
    scratch: TocScratch<'_>,
) -> usize
where
    Z: EpubZipOps,
{
    library.clear_toc();
    // Small reusable window for inflate output. The streaming tokenizer
    // consumes these chunks incrementally and never needs the whole TOC
    // resident — so any-size book is fine.
    let mut output_window = [0u8; 512];

    for toc_href in [package.nav_href, package.ncx_href].into_iter().flatten() {
        let mut toc_path = String::<MAX_ENTRY_NAME_BYTES>::new();
        if resolve_epub_href(opf_path, toc_href, &mut toc_path).is_err() {
            continue;
        }
        let Ok(toc_entry) = zip.find_entry(&toc_path, scratch.header, scratch.name) else {
            continue;
        };
        esp_println::println!(
            "epub: toc entry {} compressed={} uncompressed={}",
            toc_path.as_str(),
            toc_entry.compressed_size,
            toc_entry.uncompressed_size
        );

        let mut sink = LibraryTocSink {
            library: &mut *library,
            package,
            toc_buf: &mut *toc_buf,
            record_count: 0,
            resident_full: false,
        };
        let mut tokenizer = StreamingXmlTokenizer::new();
        let is_ncx = toc_path.as_str().ends_with(".ncx");
        let parse_ok = if is_ncx {
            let mut parser = NcxStreamParser::new();
            let feed_result = zip.read_entry_to_sink(
                toc_entry,
                scratch.compressed,
                &mut output_window,
                scratch.zip_inflate,
                &mut |chunk| {
                    tokenizer
                        .feed_ncx(chunk, &mut parser, &mut sink)
                        .map_err(|_| proto::epub::ZipError::Inflate)
                },
            );
            feed_result.is_ok() && tokenizer.finish_ncx(&mut parser, &mut sink).is_ok()
        } else {
            let mut parser = Epub3NavStreamParser::new();
            let feed_result = zip.read_entry_to_sink(
                toc_entry,
                scratch.compressed,
                &mut output_window,
                scratch.zip_inflate,
                &mut |chunk| {
                    tokenizer
                        .feed_nav(chunk, &mut parser, &mut sink)
                        .map_err(|_| proto::epub::ZipError::Inflate)
                },
            );
            feed_result.is_ok() && tokenizer.finish_nav(&mut parser, &mut sink).is_ok()
        };

        if parse_ok && (sink.record_count > 0 || sink.library.toc_count() > 0) {
            esp_println::println!(
                "epub: toc streamed {} chapter(s) ({} resident) from {} overflow={}",
                sink.record_count,
                sink.library.toc_count(),
                toc_path.as_str(),
                sink.resident_full
            );
            return sink.record_count;
        }
        esp_println::println!("epub: toc parse failed for {}", toc_path.as_str());
        sink.library.clear_toc();
    }
    esp_println::println!("epub: toc unavailable, chapters fall back to spine");
    0
}

fn href_matches_spine(href: &str, spine_href: &str) -> bool {
    let href = strip_fragment(href);
    href == spine_href
        || href.ends_with(spine_href)
        || spine_href.ends_with(href)
        || file_name(href) == file_name(spine_href)
}

fn file_name(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

struct SdFileReadAt<
    'a,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    file: File<'a, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    len: u32,
    read_ops: u32,
    read_bytes: u32,
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> ReadAt
    for SdFileReadAt<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    type Error = ();

    fn len(&mut self) -> Result<u32, Self::Error> {
        Ok(self.len)
    }

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, Self::Error> {
        if self.read_ops >= EPUB_OPEN_READ_OP_LIMIT || self.read_bytes >= EPUB_OPEN_READ_BYTE_LIMIT
        {
            esp_println::println!(
                "epub: open read budget exceeded ops={} bytes={} at offset={} request={}",
                self.read_ops,
                self.read_bytes,
                offset,
                out.len()
            );
            return Err(());
        }
        let requested = out.len();
        let remaining_budget = EPUB_OPEN_READ_BYTE_LIMIT.saturating_sub(self.read_bytes) as usize;
        let read_len = requested
            .min(EPUB_READ_AT_CHUNK_BYTES)
            .min(remaining_budget);
        if read_len == 0 {
            return Err(());
        }
        let mut last_err = None;
        for attempt in 0..3 {
            if let Err(err) = self.file.seek_from_start(offset) {
                last_err = Some(err);
                continue;
            }
            match self.file.read(&mut out[..read_len]) {
                Ok(count) => {
                    self.read_ops = self.read_ops.saturating_add(1);
                    self.read_bytes = self.read_bytes.saturating_add(count as u32);
                    if attempt > 0 {
                        esp_println::println!(
                            "epub: read_at recovered at {} len {} attempt {}",
                            offset,
                            read_len,
                            attempt + 1
                        );
                    }
                    return Ok(count);
                }
                Err(err) => {
                    last_err = Some(err);
                    for _ in 0..128 {
                        core::hint::spin_loop();
                    }
                }
            }
        }
        let err = last_err.expect("read_at records an error before retry exhaustion");
        esp_println::println!(
            "epub: read_at failed at {} len {}: {:?}",
            offset,
            read_len,
            err
        );
        Err(())
    }
}

fn resolve_epub_href(
    opf_path: &str,
    href: &str,
    out: &mut String<MAX_ENTRY_NAME_BYTES>,
) -> Result<(), ReaderCacheError> {
    out.clear();
    if href.starts_with('/') {
        out.push_str(href.trim_start_matches('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
        return Ok(());
    }
    if let Some((dir, _)) = opf_path.rsplit_once('/') {
        out.push_str(dir)
            .and_then(|_| out.push('/'))
            .map_err(|_| ReaderCacheError::EntryNameTooLong)?;
    }
    let href_no_fragment = href.split('#').next().unwrap_or(href);
    out.push_str(href_no_fragment)
        .map_err(|_| ReaderCacheError::EntryNameTooLong)
}

struct LibraryBlockSink<
    'a,
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    library: &'a mut ReaderStore,
    root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    /// The book's SECTIONS directory, opened once for the whole build.
    /// `None` when the cache directories could not be created — section
    /// writes then fail per section and the book publishes as partial.
    sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    source_identity: (u32, u32),
    font_metrics: &'a mut crate::custom_font::MetricCache,
    sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    section_count: &'a mut usize,
    total_pages: &'a mut u32,
    book_partial: &'a mut bool,
    /// Accumulated wall time spent writing section files, for the
    /// `bench: storage_build` line.
    write_micros: &'a mut u64,
    spine_index: u16,
    line: String<MAX_READER_BLOCK_TEXT>,
    /// Running ink width of `line`. `line` always starts with a style
    /// marker (or is empty), so the cursor's default font never shows
    /// through and the running width matches a from-scratch measure.
    line_ink: LineInkCursor,
    line_role: TextRole,
    line_align: TextAlign,
    line_style: FontStyle,
    pending_space: bool,
    dropping_paragraph: bool,
    stopped: bool,
    target_pages: usize,
    generate_toc_from_headings: bool,
    generated_toc_for_spine: bool,
    /// Incremental page-index cursor: each appended line advances the page
    /// records in O(1) instead of triggering a full `rebuild_page_index`
    /// re-walk of every accumulated block — O(blocks²) per section before.
    /// Hard invariant: the records must stay bit-identical to a full
    /// rebuild (they persist into section files), which the shared cursor's
    /// host tests check step-by-step against the naive walk. The carry path
    /// falls back to a full rebuild and re-adopts its cursor.
    page_cursor: ui::reading::PageIndexCursor,
    /// Latched when a page record was dropped past the `pages` capacity,
    /// mirroring the full rebuild's silent drop.
    page_overflowed: bool,
}

impl<'a, 'r, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'a, 'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// Fresh sink state for one spine-item run. Both the full EPUB build
    /// and the CONT.BIN replay construct their runs here, so the two paths
    /// start from identical state by construction — replay must reproduce
    /// the full build's layout exactly.
    #[expect(clippy::too_many_arguments)] // The sink's borrow set: counters and stores that must stay caller-owned references, not a wrapper struct built per spine item
    fn begin_spine(
        library: &'a mut ReaderStore,
        root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
        source_identity: (u32, u32),
        font_metrics: &'a mut crate::custom_font::MetricCache,
        sections: &'a mut [BookV2SectionRecord; MAX_BOOK_SECTIONS],
        section_count: &'a mut usize,
        total_pages: &'a mut u32,
        book_partial: &'a mut bool,
        write_micros: &'a mut u64,
        spine_index: u16,
        target_pages: usize,
        generate_toc_from_headings: bool,
    ) -> Self {
        library.clear_lines();
        let type_settings = library.type_settings();
        let page_box = library.page_box();
        Self {
            library,
            root,
            sections_dir,
            source_identity,
            font_metrics,
            sections,
            section_count,
            total_pages,
            book_partial,
            write_micros,
            spine_index,
            line: String::new(),
            line_ink: LineInkCursor::new(type_settings, FontStyle::Regular),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
            stopped: false,
            target_pages,
            generate_toc_from_headings,
            generated_toc_for_spine: false,
            page_cursor: ui::reading::PageIndexCursor::start(page_box),
            page_overflowed: false,
        }
    }
}

#[derive(Clone, Copy)]
enum LineInkCursor {
    BuiltIn(StyledInkCursor),
    Custom(CustomLineInkCursor),
}

impl LineInkCursor {
    fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        if settings.family == FontFamily::Custom && !display::font::builtin_custom_available() {
            Self::Custom(CustomLineInkCursor::new(settings, default_style))
        } else {
            Self::BuiltIn(StyledInkCursor::new(settings, default_style))
        }
    }

    fn width(&self) -> i16 {
        match self {
            Self::BuiltIn(cursor) => cursor.width(),
            Self::Custom(cursor) => cursor.width(),
        }
    }
}

#[derive(Clone, Copy)]
struct CustomLineInkCursor {
    advance_fp: i32,
    right: i16,
    settings: TypeSettings,
    style: FontStyle,
}

impl CustomLineInkCursor {
    const fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            advance_fp: 0,
            right: 0,
            settings,
            style: default_style,
        }
    }

    fn reset_pair(&mut self) {}

    fn push_metric(&mut self, metric: display::font::GlyphMetric) {
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
    }

    fn push_fallback(&mut self) {
        self.advance_fp += 8 << 4;
        self.right = self.right.max(fixed_ceil(self.advance_fp));
    }

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn reset_line_ink(&mut self) {
        self.line_ink = LineInkCursor::new(self.library.type_settings(), FontStyle::Regular);
    }

    fn push_line_ink_str(&mut self, text: &str) {
        match &mut self.line_ink {
            LineInkCursor::BuiltIn(cursor) => cursor.push_str(text),
            LineInkCursor::Custom(cursor) => {
                let mut chars = text.chars();
                while let Some(ch) = chars.next() {
                    if ch == STYLE_MARKER {
                        if let Some(code) = chars.next() {
                            cursor.reset_pair();
                            cursor.style =
                                display::font::style_from_marker_code(code).unwrap_or(cursor.style);
                        }
                        continue;
                    }
                    let metric = crate::custom_font::measure_char(
                        self.root,
                        self.library,
                        &mut *self.font_metrics,
                        cursor.settings.size,
                        cursor.settings.weight,
                        cursor.style,
                        ch,
                    );
                    if let Some(metric) = metric {
                        cursor.push_metric(metric);
                    } else {
                        cursor.push_fallback();
                    }
                }
            }
        }
    }

    fn line_ink_width(&self) -> i16 {
        self.line_ink.width()
    }

    /// Mirror the block just appended (at `block_count - 1`) into the page
    /// index through the shared incremental cursor — O(1) per line.
    fn note_block_appended(&mut self) {
        let index = self.library.block_count - 1;
        let placement = self.page_cursor.place_next_block(self.library, index);
        let spine = self.library.block_spine.get(index).copied().unwrap_or(0);
        ui::reading::apply_block_placement(
            placement,
            index,
            spine,
            &mut self.library.pages,
            &mut self.library.page_spine,
            &mut self.library.page_count,
            &mut self.page_overflowed,
        );
    }

    /// Bounded fix-up for `mark_last_block_paragraph_end`: the mark grows
    /// the last block by its trailing paragraph gap after placement, so
    /// re-place just that block; when it no longer fits its page, move it
    /// to a fresh one, exactly as a full rebuild would.
    fn note_last_block_grew(&mut self, index: usize) {
        let placement = self.page_cursor.replace_last_block(self.library, index);
        if placement == ui::reading::BlockPlacement::NewPage {
            let spine = self.library.block_spine.get(index).copied().unwrap_or(0);
            ui::reading::apply_last_block_move(
                index,
                spine,
                &mut self.library.pages,
                &mut self.library.page_spine,
                &mut self.library.page_count,
                &mut self.page_overflowed,
            );
        }
    }

    fn reset_page_cursor(&mut self) {
        self.page_cursor = ui::reading::PageIndexCursor::start(self.library.page_box());
        self.page_overflowed = false;
    }

    fn finish_spine(&mut self, partial: bool) {
        flush_styled_preview_line(self, true);
        self.flush_section(partial || self.stopped, false);
    }

    fn flush_section(&mut self, partial: bool, carry_incomplete: bool) -> bool {
        if self.library.block_count() == 0 || self.library.page_count == 0 {
            self.library.clear_lines();
            self.reset_page_cursor();
            return true;
        }
        if *self.section_count >= self.sections.len() {
            *self.book_partial = true;
            self.stopped = true;
            return false;
        }
        if partial {
            *self.book_partial = true;
        }

        // Intermediate sections end on a whole page: the half-finished final
        // page carries into the next section rather than being written as a
        // short, half-empty page the reader would stop on mid-chapter. The
        // last section of a chapter (finish_spine) keeps its trailing page —
        // that is the genuine end of the text.
        let full_blocks = self.library.block_count();
        let full_text = self.library.text_len;
        let full_pages = self.library.page_count;
        let carry_first = if carry_incomplete && full_pages > 1 {
            let cut = self.library.pages[full_pages - 1].first_block as usize;
            (cut > 0 && cut < full_blocks).then_some(cut)
        } else {
            None
        };
        if let Some(cut) = carry_first {
            self.library.block_count = cut;
            self.library.page_count = full_pages - 1;
            self.library.text_len = self.library.blocks[cut].text_offset as usize;
        }

        self.library.set_cached_spine(self.spine_index);
        self.library.set_section_partial(partial);
        let section_id = (*self.section_count).min(u16::MAX as usize) as u16;
        let write_started = Instant::now();
        let wrote = match self.sections_dir {
            Some(sections) => reader_cache_files::write_v2_section_cache_in(
                sections,
                self.source_identity,
                section_id,
                self.library,
            ),
            None => false,
        };
        *self.write_micros += write_started.elapsed().as_micros();
        if !wrote {
            *self.book_partial = true;
        }
        self.sections[*self.section_count] = BookV2SectionRecord {
            section: section_id,
            spine: self.spine_index,
            start_page: *self.total_pages,
            page_count: self.library.page_count.min(u16::MAX as usize) as u16,
            partial,
        };
        *self.total_pages = (*self.total_pages).saturating_add(self.library.page_count as u32);
        *self.section_count += 1;

        match carry_first {
            Some(cut) => {
                self.library.block_count = full_blocks;
                self.library.text_len = full_text;
                self.library.carry_last_page(cut);
                // Full rebuild on the carry path: the carried blocks were
                // rebased, so re-derive the page records and adopt the
                // walk's cursor for the appends that follow.
                let (cursor, overflowed) = reader_layout::rebuild_page_index(self.library);
                self.page_cursor = cursor;
                self.page_overflowed = overflowed;
            }
            None => {
                self.library.clear_lines();
                self.reset_page_cursor();
            }
        }
        true
    }

    fn flush_if_full(&mut self) {
        if self.library.page_count >= self.target_pages
            || self.library.block_count() >= self.library.block_capacity().saturating_sub(4)
            || self.library.text_capacity_reached()
        {
            flush_styled_preview_line(self, false);
            self.flush_section(false, true);
        }
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> XhtmlBlockSink
    for LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError> {
        if self.stopped {
            return Err(XhtmlError::TooManyRuns);
        }
        self.flush_if_full();
        push_styled_preview_fragment(
            self,
            text,
            preview_style_for_proto_style(style, role),
            role,
            align,
            paragraph_end,
        );
        self.flush_if_full();
        Ok(())
    }
}

fn preview_style_for_proto_style(style: proto::text::FontStyle, role: TextRole) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => {
            if matches!(
                role,
                TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
            ) {
                FontStyle::Bold
            } else {
                FontStyle::Regular
            }
        }
    }
}

fn push_styled_preview_fragment<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    text: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.dropping_paragraph {
        if paragraph_end {
            sink.dropping_paragraph = false;
            sink.pending_space = false;
        }
        return;
    }
    let starts_with_space = text
        .chars()
        .next()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(|ch| ch.is_whitespace())
        .unwrap_or(false);
    let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
    push_normalized_decoded(text, &mut normalized);
    trim_trailing_space(&mut normalized);
    if !sanitize_preview_block(&mut normalized) {
        sink.dropping_paragraph = !paragraph_end;
        sink.pending_space = false;
        return;
    }
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }

    normalize_decorative_separator(&mut normalized);
    let align = block_align_for(align, normalized.as_str(), role);
    let page_box = sink.library.page_box();
    let x = page_box.x_for(role);
    let max_x = page_box.right;
    // Book-style first-line indent: a Body paragraph's opening line wraps
    // against a narrower column. Only Left/Justify Body takes it, matching
    // `ui::reading::block_first_line_indent`, so the built line breaks agree
    // with how the page later draws them.
    let indent = if matches!(role, TextRole::Body)
        && matches!(align, TextAlign::Left | TextAlign::Justify)
    {
        reader_layout::paragraph_indent(sink.library.type_settings().size)
    } else {
        0
    };

    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_styled_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }

    let mut first_word = true;
    for word in normalized.split_whitespace() {
        let attach = is_leading_punctuation_word(word) && !sink.line.is_empty();
        let leading_space = !sink.line.is_empty()
            && !attach
            && (sink.pending_space || !first_word || starts_with_space);
        let kept_len = sink.line.len();
        let kept_ink = sink.line_ink;
        let line_was_empty = sink.line.is_empty();
        if append_styled_word(&mut sink.line, word, style, sink.line_style, leading_space).is_err()
        {
            sink.line.truncate(kept_len);
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, sink.line_style, false);
            let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
            let _ = measure.push_str(sink.line.as_str());
            sink.push_line_ink_str(measure.as_str());
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
            first_word = false;
            continue;
        }
        let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
        let _ = measure.push_str(&sink.line[kept_len..]);
        sink.push_line_ink_str(measure.as_str());

        // The line in progress opens a paragraph while no line of it has
        // flushed yet: the previous block still closes a paragraph. Once the
        // first line flushes (as a non-end block) this goes false and the
        // continuation lines wrap at the full width.
        let opens_paragraph = sink.library.block_count == 0
            || sink
                .library
                .block_paragraph_end
                .get(sink.library.block_count.wrapping_sub(1))
                .copied()
                .unwrap_or(true);
        let x_eff = if opens_paragraph { x + indent } else { x };
        if !line_was_empty
            && sink.line_ink_width() + x_eff + reader_layout::READER_WRAP_SAFETY > max_x
        {
            sink.line.truncate(kept_len);
            sink.line_ink = kept_ink;
            flush_styled_preview_line(sink, false);
            let _ = append_styled_word(&mut sink.line, word, style, sink.line_style, false);
            let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
            let _ = measure.push_str(sink.line.as_str());
            sink.push_line_ink_str(measure.as_str());
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        } else {
            sink.line_role = role;
            sink.line_align = align;
            sink.line_style = style;
            sink.pending_space = false;
        }
        first_word = false;
    }

    sink.pending_space |= ends_with_space;
    if paragraph_end {
        flush_styled_preview_line(sink, true);
    }
}

fn append_styled_word<const N: usize>(
    line: &mut String<N>,
    word: &str,
    style: FontStyle,
    current_style: FontStyle,
    leading_space: bool,
) -> Result<(), ()> {
    if leading_space {
        line.push(' ').map_err(|_| ())?;
    }
    // Emit a style marker only when the run actually changes. Plain prose
    // (the bulk of a book) then carries no markers at all -- ~25-30% more
    // text per section and proportionally fewer chunks against the same
    // 16 KB arena. The draw and measure paths both key off the running
    // font, so a dropped redundant marker is a no-op; and because each line
    // draws from a Regular default and `current_style` is reset to Regular
    // on every flush, a continuation line still re-marks a non-Regular
    // opening word.
    if style != current_style {
        append_style_marker(line, style)?;
    }
    line.push_str(word).map_err(|_| ())
}

fn append_style_marker<const N: usize>(line: &mut String<N>, style: FontStyle) -> Result<(), ()> {
    line.push(reader_layout::STYLE_MARKER).map_err(|_| ())?;
    line.push(reader_layout::style_marker_code(style))
        .map_err(|_| ())
}

fn flush_styled_preview_line<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    paragraph_end: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.line.is_empty() {
        if paragraph_end {
            let count = sink.library.block_count;
            let changed = count > 0 && !sink.library.block_paragraph_end[count - 1];
            sink.library.mark_last_block_paragraph_end();
            if changed {
                // Height-only change: the trailing paragraph gap grew the
                // last block after it was placed, so re-place just that
                // block instead of rebuilding the whole page index.
                sink.note_last_block_grew(count - 1);
            }
        }
        return;
    }

    let line = sink.line.clone();
    let role = sink.line_role;
    let align = sink.line_align;
    let style = reader_layout::first_styled_line_style(line.as_str()).unwrap_or(FontStyle::Regular);
    if sink.generate_toc_from_headings
        && !sink.generated_toc_for_spine
        && matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
    {
        let mut title = String::<160>::new();
        push_plain_styled_text(line.as_str(), &mut title);
        trim_trailing_space(&mut title);
        if !title.is_empty()
            && sink.library.push_toc_record(
                title.as_str(),
                heading_toc_level(role),
                sink.spine_index as i16,
            )
        {
            sink.generated_toc_for_spine = true;
        }
    }
    let mut appended_from = sink.library.block_count;
    if !sink.library.push_line_block(
        line.as_str(),
        style,
        role,
        align,
        paragraph_end,
        sink.spine_index,
    ) {
        // The section arena (text bytes or the block table) just filled.
        // Flush what we have to a section file and retry the line into a
        // fresh arena, so a long chapter chunks and continues instead of
        // losing its tail. (At the book-wide section ceiling flush_section
        // refuses and sets book_partial; the line is then genuinely
        // dropped, which is the separate whole-book limit.)
        sink.flush_section(false, true);
        appended_from = sink.library.block_count;
        let _ = sink.library.push_line_block(
            line.as_str(),
            style,
            role,
            align,
            paragraph_end,
            sink.spine_index,
        );
    }
    if sink.library.block_count > appended_from {
        // push_line_block can also no-op (empty trim, full block table), so
        // only an actual append advances the page cursor.
        sink.note_block_appended();
    }
    sink.line.clear();
    sink.reset_line_ink();
    sink.line_style = FontStyle::Regular;
    sink.pending_space = false;
}

fn heading_toc_level(role: TextRole) -> u8 {
    match role {
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::Body | TextRole::BlockQuote => 1,
    }
}

fn push_plain_styled_text<const N: usize>(styled: &str, out: &mut String<N>) {
    let mut skip_style_code = false;
    for ch in styled.chars() {
        if skip_style_code {
            skip_style_code = false;
            continue;
        }
        if ch == reader_layout::STYLE_MARKER {
            skip_style_code = true;
            continue;
        }
        let _ = out.push(ch);
    }
}

fn is_leading_punctuation_word(word: &str) -> bool {
    word.chars()
        .next()
        .map(|ch| {
            matches!(
                ch,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
            )
        })
        .unwrap_or(false)
}

fn block_align_for(run_align: TextAlign, block: &str, role: TextRole) -> TextAlign {
    if run_align == TextAlign::Center
        || matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
        || is_decorative_separator(block)
    {
        TextAlign::Center
    } else {
        run_align
    }
}

fn normalize_decorative_separator<const N: usize>(block: &mut String<N>) {
    if !is_decorative_separator(block.as_str()) {
        return;
    }
    block.clear();
    let _ = block.push_str("* * *");
}

fn is_decorative_separator(text: &str) -> bool {
    let mut saw_mark = false;
    let mut mark_count = 0u8;
    for ch in text.chars() {
        if ch == '*' {
            saw_mark = true;
            mark_count = mark_count.saturating_add(1);
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        return false;
    }
    saw_mark && mark_count >= 3
}

fn push_normalized_decoded<const N: usize>(text: &str, out: &mut String<N>) {
    let mut previous_space = true;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if let Some(decoded) = decode_html_entity(rest) {
            if decoded.is_whitespace() {
                if !previous_space && out.push(' ').is_err() {
                    break;
                }
                previous_space = true;
            } else if push_normalized_char(decoded, out).is_err() {
                break;
            } else {
                previous_space = false;
            }
            cursor += rest.find(';').map(|index| index + 1).unwrap_or(1);
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else if push_normalized_char(ch, out).is_err() {
            break;
        } else {
            previous_space = false;
        }
        cursor += ch.len_utf8();
    }
}

fn push_normalized_char<const N: usize>(ch: char, out: &mut String<N>) -> Result<(), ()> {
    match ch {
        '\u{00A0}' => out.push(' ').map_err(|_| ()),
        ch if ch as u32 <= u16::MAX as u32 => out.push(ch).map_err(|_| ()),
        _ => out.push('?').map_err(|_| ()),
    }
}

fn is_epub_titlepage_label(text: &str) -> bool {
    let lower = LowerAscii::<128>::new(text);
    lower.starts_with(": ")
        || lower.eq("title")
        || lower.eq("author")
        || lower.eq("creator")
        || lower.eq("language")
        || lower.eq("english")
        || lower.eq("english:")
        || lower.eq("release date")
        || lower.eq("original publication")
        || lower.starts_with("most recently updated")
        || lower.starts_with("other information")
        || lower.starts_with("other formats")
        || lower.starts_with("credits")
        || lower.starts_with("produced by")
        || lower.starts_with("transcribed from")
        || lower.starts_with("project gutenberg")
        || lower.starts_with("the project gutenberg")
}

fn sanitize_preview_block<const N: usize>(block: &mut String<N>) -> bool {
    trim_trailing_space(block);
    trim_leading_space(block);
    if block.is_empty() {
        return false;
    }
    if is_epub_titlepage_label(block) || contains_gutenberg_metadata(block.as_str()) {
        return false;
    }
    if is_decorative_separator(block.as_str()) {
        normalize_decorative_separator(block);
        return true;
    }
    if let Some(rest) = decorative_prefix_rest(block.as_str()) {
        if rest.is_empty() {
            normalize_decorative_separator(block);
            return true;
        }
        if is_epub_titlepage_label(rest) || contains_gutenberg_metadata(rest) {
            return false;
        }
    }
    true
}

fn decorative_prefix_rest(text: &str) -> Option<&str> {
    let mut mark_count = 0u8;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '*' {
            mark_count = mark_count.saturating_add(1);
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if mark_count >= 3 {
        Some(text[end..].trim())
    } else {
        None
    }
}

fn contains_gutenberg_metadata(text: &str) -> bool {
    let lower = LowerAscii::<160>::new(text);
    lower.contains("most recently updated")
        || lower.contains("project gutenberg ebook")
        || lower.contains("start of the project gutenberg")
        || lower.contains("end of the project gutenberg")
        || lower.contains("other information and formats")
        || lower.contains("this ebook is for the use of anyone")
        || lower.contains("project gutenberg license")
        || lower.contains("www.gutenberg.org")
        || lower.contains("laws of the country where you are located")
}

fn trim_trailing_space<const N: usize>(text: &mut String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
}

fn trim_leading_space<const N: usize>(text: &mut String<N>) {
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

struct LowerAscii<const N: usize> {
    text: String<N>,
}

impl<const N: usize> LowerAscii<N> {
    fn new(input: &str) -> Self {
        let mut text = String::new();
        for byte in input.bytes() {
            if text.push((byte as char).to_ascii_lowercase()).is_err() {
                break;
            }
        }
        Self { text }
    }

    fn eq(&self, other: &str) -> bool {
        self.text.as_str() == other
    }

    fn starts_with(&self, other: &str) -> bool {
        self.text.as_str().starts_with(other)
    }

    fn ends_with(&self, other: &str) -> bool {
        self.text.as_str().ends_with(other)
    }

    fn contains(&self, other: &str) -> bool {
        self.text.as_str().contains(other)
    }

    fn word_eq(&self, other: &str) -> bool {
        self.text
            .as_str()
            .split_ascii_whitespace()
            .any(|word| word == other)
    }
}
