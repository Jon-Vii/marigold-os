use crate::display_flush::Epd;
use crate::reader_cache_files;
use crate::reader_cache_files::{BookIndexLoadResult, CacheLoadResult};
use crate::reader_layout;
use crate::reader_store::{
    source_hash, BookLoadStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
    MAX_READER_BLOCK_TEXT,
};
use crate::sd_session::{self, SdSessionError};
use display::font::{fixed_ceil, fixed_round, FontFamily, FontStyle, TypeSettings};
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;
use heapless::String;
use proto::book::BookId;
use proto::cache::BookV2SectionRecord;
use proto::epub::{
    parse_opf_streamed, strip_fragment, Epub3NavStreamParser, EpubTocSink, EpubZipOps,
    NcxStreamParser, ReadAt, StreamingXmlTokenizer, TocError, XhtmlBlockSink,
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
    requested_chapter: u16,
    target_pages: usize,
    requested_spine: Option<u16>,
    scratch: &mut ReaderCacheScratch<'_>,
    font_metrics: &mut crate::custom_font::MetricCache,
) {
    esp_println::println!(
        "epub: cache open index {} chapter {} target {}",
        index,
        requested_chapter,
        target_pages
    );
    // Before the first nonempty spine there is no durable index yet. Retain
    // only that tiny same-book cursor across task turns; all durable prefixes
    // are reloaded and validated from their index.
    let empty_next = if library.loaded_index == Some(index)
        && library.reader_status() == BookLoadStatus::Loading
        && library.book_section_count == 0
    {
        library.book_next_spine()
    } else {
        None
    };
    library.begin_book_load();
    library.set_book_next_spine(empty_next);

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
            requested_spine,
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
    library.loaded_index = Some(index);
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
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
    requested_chapter: u16,
    target_pages: usize,
    requested_spine: Option<u16>,
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
    if try_load_v2_book_cache(
        root,
        cache_key.as_str(),
        source_identity,
        target_pages as u32,
        requested_spine,
        library,
        Instant::now(),
        "fast",
        scratch.xhtml,
    ) {
        return BookLoadStatus::Ready;
    }

    if in_books_dir {
        let load_result = match root.open_dir("BOOKS") {
            Ok(books) => match books.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
                Ok(file) => Some(build_or_load_epub_cache_from_file(
                    file,
                    root,
                    &display_name,
                    requested_chapter,
                    target_pages,
                    requested_spine,
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
                requested_spine,
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
    }
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
#[allow(clippy::too_many_arguments)]
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
    requested_spine: Option<u16>,
    library: &mut ReaderStore,
    started: Instant,
    label: &str,
    reflow_scratch: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut result =
        reader_cache_files::load_v2_book_index(root, cache_key, source_identity, library);
    if result == BookIndexLoadResult::SpacingMismatch {
        if !reader_cache_files::reflow_book_spacing(
            root,
            cache_key,
            source_identity,
            library,
            reflow_scratch,
        ) {
            library.clear_book_index();
            return false;
        }
        esp_println::println!(
            "bench: storage_reflow elapsed_ms={} pages={} sections={} key={}",
            started.elapsed().as_millis(),
            library.book_total_pages,
            library.book_section_count,
            cache_key
        );
        result = BookIndexLoadResult::Hit;
    }
    match result {
        BookIndexLoadResult::Hit => {
            // A durable prefix is a resume point for preparation, not a ready
            // book. Returning a cache hit here would keep replaying that prefix
            // without ever appending its remaining spines.
            if library.book_next_spine().is_some() {
                return false;
            }
            // BOOK is the commit record. Reconstruct a generated TOC sidecar
            // after interrupted publication, rather than trusting an older file.
            if library.book_generated_toc()
                && !generated_toc_is_current(root, cache_key, source_identity, library)
                && !write_generated_toc(root, cache_key, source_identity, library, reflow_scratch)
            {
                return false;
            }
            let Some(requested_global_page) =
                library.requested_page(requested_global_page, requested_spine)
            else {
                return false;
            };
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
                    // A missing/corrupt page within the known prefix needs a
                    // replacement, not another append to that damaged prefix.
                    library.clear_book_index();
                    false
                }
            }
        }
        BookIndexLoadResult::Invalid | BookIndexLoadResult::SpacingMismatch => {
            esp_println::println!("epub: v2 {label} book index invalid");
            false
        }
        BookIndexLoadResult::Miss => {
            esp_println::println!("epub: v2 {label} book index miss");
            false
        }
    }
}

fn generated_toc_is_current<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    source: (u32, u32),
    library: &mut ReaderStore,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !reader_cache_files::load_v2_toc_into_text(root, key, source, library, 0)
        || library.overview_chapter_count() != library.toc_count()
    {
        return false;
    }
    for index in 0..library.toc_count() {
        let toc = library.toc[index];
        let expected =
            proto::cache::toc_chapter_record(library.toc_title(index), toc.level, toc.spine_index);
        if library.overview_title_at(index) != expected.title_str()
            || library.overview_spine_at(index) != expected.spine_index
            || library.overview_level_at(index) != expected.level
        {
            return false;
        }
    }
    true
}

fn write_generated_toc<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    key: &str,
    source: (u32, u32),
    library: &ReaderStore,
    scratch: &mut [u8],
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let count = library.toc_count();
    let len = count * proto::cache::TOC_CHAPTER_RECORD_BYTES;
    if len > scratch.len() {
        return false;
    }
    for index in 0..count {
        let toc = library.toc[index];
        let record =
            proto::cache::toc_chapter_record(library.toc_title(index), toc.level, toc.spine_index);
        let offset = index * proto::cache::TOC_CHAPTER_RECORD_BYTES;
        if proto::cache::encode_toc_chapter(
            &record,
            &mut scratch[offset..offset + proto::cache::TOC_CHAPTER_RECORD_BYTES],
        )
        .is_err()
        {
            return false;
        }
    }
    reader_cache_files::write_v2_toc_file(root, key, source, count, &scratch[..len])
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
        ReaderCacheError::Epub(proto::epub::EpubError::TooManySpineItems) => "OPF SPINE",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpfPath) => "NO OPF",
        ReaderCacheError::Epub(proto::epub::EpubError::MissingOpf) => "NO OPF FILE",
        ReaderCacheError::Epub(proto::epub::EpubError::Utf8) => "OPF UTF8",
        ReaderCacheError::Epub(proto::epub::EpubError::Zip(_)) => "OPF ZIP",
        ReaderCacheError::Epub(_) => "OPF",
        ReaderCacheError::Xhtml(proto::epub::XhtmlError::TooManyRuns) => "TEXT FULL",
        ReaderCacheError::Utf8 => "UTF8",
        ReaderCacheError::MissingOpfPath => "NO OPF",
        ReaderCacheError::MissingSpine => "NO SPINE",
        ReaderCacheError::NoBodyText => "NO BODY TEXT",
        ReaderCacheError::CacheWrite => "CACHE WRITE",
        ReaderCacheError::CacheRead => "CACHE READ",
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
    MissingOpfPath,
    MissingSpine,
    NoBodyText,
    CacheWrite,
    CacheRead,
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
    _requested_chapter: u16,
    target_pages: usize,
    requested_spine: Option<u16>,
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
        requested_spine,
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
    requested_spine: Option<u16>,
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
    let resume_next = library.book_next_spine();
    let resumed_sections = if resume_next.is_some() {
        library.book_section_count
    } else {
        0
    };
    let section_base = if resumed_sections > 0 {
        (library.book_sections[0].section / MAX_BOOK_SECTIONS as u16) * MAX_BOOK_SECTIONS as u16
    } else {
        reader_cache_files::prepare_book_replacement(root, cache_key)
            .ok_or(ReaderCacheError::CacheWrite)?
    };
    let io_start = crate::sd_session::sd_stats::snapshot();
    let mut section_write_micros: u64 = 0;
    esp_println::println!("epub: stage ParseContainerAndOpf");
    let container_entry = zip.find_entry("META-INF/container.xml", scratch.header, scratch.name)?;
    let container_len = zip.read_entry_streamed(
        container_entry,
        scratch.compressed,
        scratch.container,
        &mut *scratch.zip_inflate,
    )?;
    let container_xml = core::str::from_utf8(&scratch.container[..container_len])
        .map_err(|_| ReaderCacheError::Utf8)?;
    let opf_path = find_full_path(container_xml).ok_or(ReaderCacheError::MissingOpfPath)?;

    let opf_entry = zip.find_entry(opf_path, scratch.header, scratch.name)?;
    esp_println::println!(
        "epub: opf compressed={} uncompressed={}",
        opf_entry.compressed_size,
        opf_entry.uncompressed_size
    );
    // The OPF streams through the idle XHTML window (twice: spine ids, then
    // the items they name) and only the strings the package needs land in
    // the OPF arena, so an OPF of any size parses without truncation.
    let package = {
        let zip = &mut zip;
        let compressed = &mut *scratch.compressed;
        let window = &mut *scratch.xhtml;
        let inflate = &mut *scratch.zip_inflate;
        parse_opf_streamed(
            |feed| {
                zip.read_entry_to_sink(opf_entry, compressed, window, inflate, &mut |chunk| {
                    feed(chunk).map_err(|_| proto::epub::ZipError::Inflate)
                })
                .map_err(proto::epub::EpubError::from)
            },
            scratch.opf,
            BookId(2),
            source_path,
            0,
            opf_path,
        )?
    };
    esp_println::println!(
        "epub: opf parsed after {} ms (spine={} truncated={} arena={})",
        open_started.elapsed().as_millis(),
        package.spine.len(),
        package.spine_truncated,
        package.opf_text.len()
    );

    library.set_book_labels(package.meta.title, package.meta.author);
    library.clear_cover();
    if resumed_sections > 0 {
        // The prefix index and existing TOC sidecar remain authoritative.
    } else if zip.is_forward_only() {
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

    esp_println::println!("epub: stage BuildV2BookCache");
    let spine_started = Instant::now();
    let mut xhtml_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    scratch.book_sections.fill(EMPTY_BOOK_SECTION_RECORD);
    let sections = &mut *scratch.book_sections;
    sections[..resumed_sections].copy_from_slice(&library.book_sections[..resumed_sections]);
    let mut section_count = resumed_sections;
    let mut total_pages = if resumed_sections > 0 {
        library.book_total_pages
    } else {
        0
    };
    let mut saw_spine = false;
    // A spine clipped at MAX_SPINE_ITEMS means the tail chapters were dropped at
    // parse, so the book is partial even if every kept section caches cleanly.
    let mut book_partial =
        package.spine_truncated || (resumed_sections > 0 && library.book_cache_partial);
    let visible_page_capacity = library.page_capacity().max(1);
    let generate_toc_from_headings = if resumed_sections > 0 {
        library.book_generated_toc()
    } else {
        library.toc_count() == 0
    };
    library.set_book_generated_toc(generate_toc_from_headings);
    let start_spine_index = package
        .text_reference_href
        .and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| href_matches_spine(href, item.href.of(package.opf_text)))
        })
        .unwrap_or_else(|| inferred_start_spine_index(&package));

    let start_spine_index = resume_next.map(usize::from).unwrap_or(start_spine_index);
    let mut next_spine = None;
    let mut processed_spine = false;
    // Each call processes at most one spine. No ZIP/parser/FAT borrow survives
    // the return to the display task, which handles input/render/sleep first.
    // The SECTIONS directory stays open for the whole spine walk; every
    // section flush is one file open instead of a four-level dir walk.
    reader_cache_files::with_v2_sections_dir(root, cache_key, |sections_dir| {
        for (spine_index, spine) in package.spine.iter().enumerate().filter(|(index, item)| {
            *index >= start_spine_index
                && !item.href.is_empty()
                && !spine_item_is_navigation(item, &package)
        }) {
            if processed_spine {
                next_spine = Some(spine_index as u16);
                break;
            }
            processed_spine = true;
            if section_count >= sections.len() {
                book_partial = true;
                break;
            }
            saw_spine = true;
            library.clear_lines();
            resolve_epub_href(opf_path, spine.href.of(package.opf_text), &mut xhtml_path)?;
            esp_println::println!("epub: find spine {}", xhtml_path.as_str());
            let xhtml_entry = match zip.find_entry(&xhtml_path, scratch.header, scratch.name) {
                Ok(entry) => entry,
                Err(proto::epub::ZipError::Io) => return Err(proto::epub::ZipError::Io.into()),
                Err(err) => {
                    // A dangling spine reference names no content to lose;
                    // skip it (a retryable storage fault returned above).
                    esp_println::println!(
                        "epub: spine {} not found ({:?}); skipping it",
                        xhtml_path.as_str(),
                        err
                    );
                    continue;
                }
            };
            esp_println::println!(
                "epub: spine {} compressed={} uncompressed={}",
                xhtml_path.as_str(),
                xhtml_entry.compressed_size,
                xhtml_entry.uncompressed_size
            );
            let type_settings = library.type_settings();
            let mut sink = LibraryBlockSink {
                library,
                root,
                sections_dir,
                source_identity,
                section_base,
                font_metrics: &mut *font_metrics,
                sections: &mut *sections,
                section_count: &mut section_count,
                total_pages: &mut total_pages,
                book_partial: &mut book_partial,
                write_micros: &mut section_write_micros,
                spine_index: spine_index.min(u16::MAX as usize) as u16,
                line: String::new(),
                line_ink: LineInkCursor::new(type_settings, FontStyle::Regular),
                line_role: TextRole::Body,
                line_align: TextAlign::Justify,
                line_style: FontStyle::Regular,
                pending_space: false,
                dropping_paragraph: false,
                stopped: false,
                target_pages: visible_page_capacity,
                generate_toc_from_headings,
                generated_toc_for_spine: false,
                pages_dirty: true,
                write_failed: false,
            };
            // Stream the whole member through the resumable block parser in
            // bounded windows: spine XHTML of any size decodes completely, with
            // no 24 KB prefix truncation. The parser's in-body assumption is
            // sniffed from the first decoded window, mirroring the
            // whole-document contains() check.
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
                        .feed_xhtml_blocks(chunk, parser, None, &mut sink)
                        .map_err(|err| {
                            parse_error = Some(err);
                            proto::epub::ZipError::Inflate
                        })
                },
            );
            match read_result {
                Ok(()) => {
                    if let Some(parser) = parser.as_mut() {
                        if let Err(err) = tokenizer.finish_xhtml_blocks(parser, &mut sink) {
                            if !sink.stopped {
                                return Err(err.into());
                            }
                        }
                    }
                }
                Err(_) if parse_error.is_some() => {
                    let err = parse_error.take().expect("parse error recorded");
                    if sink.stopped {
                        esp_println::println!(
                            "epub: bounded open stopped at spine {} after {} section(s): {:?}",
                            spine_index,
                            *sink.section_count,
                            err
                        );
                    } else {
                        return Err(err.into());
                    }
                }
                // A storage fault is worth retrying, so it still fails the
                // step. Anything else is a damaged member: keep what parsed,
                // mark the book partial, and carry on with the next chapter
                // rather than refusing to open the whole book.
                Err(proto::epub::ZipError::Io) => {
                    return Err(proto::epub::ZipError::Io.into());
                }
                Err(err) => {
                    esp_println::println!(
                        "epub: spine {} unreadable ({:?}); skipping it",
                        xhtml_path.as_str(),
                        err
                    );
                    *sink.book_partial = true;
                }
            }
            sink.finish_spine(false);
            if sink.write_failed {
                return Err(ReaderCacheError::CacheWrite);
            }
        }
        Ok::<(), ReaderCacheError>(())
    })?;

    if section_count >= sections.len() && next_spine.is_some() {
        book_partial = true;
        next_spine = None;
    }
    library.set_book_next_spine(next_spine);
    if section_count > 0 && total_pages > 0 {
        let sections_slice = &sections[..section_count];
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
            return Err(ReaderCacheError::CacheWrite);
        }
        if generate_toc_from_headings
            && !write_generated_toc(root, cache_key, source_identity, library, scratch.xhtml)
        {
            return Err(ReaderCacheError::CacheWrite);
        }

        library.set_book_index(total_pages, book_partial, sections_slice);
        library.set_book_next_spine(next_spine);
        let resolved_page = library.requested_page(requested_global_page, requested_spine);
        // During a distant-page/chapter request, leave a real committed prefix
        // resident while the task schedules the next independent spine step.
        let loaded_page = resolved_page.unwrap_or(0);
        match reader_cache_files::load_v2_section_by_global_page(
            root,
            cache_key,
            source_identity,
            loaded_page,
            library,
        ) {
            CacheLoadResult::Hit { .. } => {}
            _ => return Err(ReaderCacheError::CacheRead),
        }
        reader_layout::rebuild_toc_page_targets(library);
        refresh_chapter_tracking(root, cache_key, source_identity, loaded_page, library);
        let cover = reader_cache_files::load_v2_cover_cache(root, cache_key, library);
        esp_println::println!("epub: stage PublishLoaded");
        esp_println::println!(
            "epub: book cache step ready after {} ms (pages={} sections={} partial={} cover={:?} key {} next_spine={:?})",
            open_started.elapsed().as_millis(),
            total_pages,
            section_count,
            book_partial,
            cover,
            cache_key,
            next_spine
        );
        let io = crate::sd_session::sd_stats::snapshot().since(io_start);
        esp_println::println!(
            "bench: storage_build elapsed_ms={} spine_ms={} write_ms={} sections={} pages={} rd_calls={} rd_blocks={} wr_calls={} wr_blocks={} key={} complete={}",
            open_started.elapsed().as_millis(),
            spine_started.elapsed().as_millis(),
            section_write_micros / 1000,
            section_count,
            total_pages,
            io.read_calls,
            io.read_blocks,
            io.write_calls,
            io.write_blocks,
            cache_key,
            next_spine.is_none()
        );
        Ok(())
    } else if next_spine.is_some() {
        Ok(())
    } else if saw_spine {
        Err(ReaderCacheError::NoBodyText)
    } else {
        Err(ReaderCacheError::MissingSpine)
    }
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
    // TOC hrefs are relative to the nav/NCX file, spine hrefs to the OPF, so
    // compare by path suffix -- but only on a `/` boundary, or "11.xhtml"
    // would claim the spine item "1.xhtml".
    href == spine_href
        || path_ends_with(href, spine_href)
        || path_ends_with(spine_href, href)
        || file_name(href) == file_name(spine_href)
}

fn path_ends_with(path: &str, suffix: &str) -> bool {
    !suffix.is_empty()
        && path.ends_with(suffix)
        && (path.len() == suffix.len() || path.as_bytes()[path.len() - suffix.len() - 1] == b'/')
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

fn find_full_path(xml: &str) -> Option<&str> {
    let key = "full-path";
    let start = xml.find(key)?;
    let after_key = &xml[start + key.len()..];
    let equals = after_key.find('=')?;
    let after_equals = after_key[equals + 1..].trim_start();
    let quote = after_equals.as_bytes().first().copied()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value = &after_equals[1..];
    let end = value.as_bytes().iter().position(|byte| *byte == quote)?;
    Some(&value[..end])
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
    section_base: u16,
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
    /// Only appends and final paragraph-end marks occur between updates.
    /// Completed pages stay valid; the dirty trailing page is repaginated.
    pages_dirty: bool,
    write_failed: bool,
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
                crate::custom_font::for_each_metric(
                    self.root,
                    self.library,
                    &mut *self.font_metrics,
                    cursor.settings.size,
                    cursor.settings.weight,
                    cursor.style,
                    text,
                    |style, metric| {
                        if cursor.style != style {
                            cursor.reset_pair();
                            cursor.style = style;
                        }
                        if let Some(metric) = metric {
                            cursor.push_metric(metric);
                        } else {
                            cursor.push_fallback();
                        }
                    },
                );
            }
        }
    }

    fn line_ink_width(&self) -> i16 {
        self.line_ink.width()
    }

    fn rebuild_pages_if_dirty(&mut self) {
        if self.pages_dirty {
            reader_layout::extend_page_index(self.library);
            self.pages_dirty = false;
        }
    }

    fn finish_spine(&mut self, partial: bool) {
        flush_styled_preview_line(self, true);
        self.flush_section(partial || self.stopped, false);
    }

    fn flush_section(&mut self, partial: bool, carry_incomplete: bool) -> bool {
        self.rebuild_pages_if_dirty();
        if self.library.block_count() == 0 || self.library.page_count == 0 {
            self.library.clear_lines();
            self.pages_dirty = true;
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

        self.library.set_cached_spine(self.spine_index);
        self.library.set_section_partial(partial);
        let section_id = self.section_base + (*self.section_count) as u16;
        let write_started = Instant::now();
        let mut written_pages = 0;
        let wrote = reader_layout::flush_page_window(self.library, carry_incomplete, |window| {
            written_pages = window.page_count;
            match self.sections_dir {
                Some(sections) => reader_cache_files::write_v2_section_cache_in(
                    sections,
                    self.source_identity,
                    section_id,
                    window,
                ),
                None => false,
            }
        });
        *self.write_micros += write_started.elapsed().as_micros();
        if !wrote {
            self.write_failed = true;
            self.stopped = true;
            return false;
        }
        self.sections[*self.section_count] = BookV2SectionRecord {
            section: section_id,
            spine: self.spine_index,
            start_page: *self.total_pages,
            page_count: written_pages.min(u16::MAX as usize) as u16,
            partial,
        };
        *self.total_pages = (*self.total_pages).saturating_add(written_pages as u32);
        *self.section_count += 1;
        self.pages_dirty = false;
        true
    }

    fn flush_if_full(&mut self) {
        self.rebuild_pages_if_dirty();
        if self.library.page_count >= self.target_pages
            || self.library.block_count() >= self.library.block_capacity().saturating_sub(4)
            || self.library.text_capacity_reached()
        {
            // The line still being filled is not pushed here: the section
            // carries its unfinished last page forward, and the line keeps
            // collecting words until it wraps naturally. Pushing it early
            // ended a line mid-paragraph at most section boundaries.
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
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false);
    let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
    push_normalized(text, &mut normalized);
    trim_trailing_space(&mut normalized);
    // A fragment with no visible text only carries spacing. Handle it before
    // the paragraph sanitizer, which treats an empty block as "drop the rest
    // of this paragraph".
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }
    if !sanitize_preview_block(&mut normalized) {
        sink.dropping_paragraph = !paragraph_end;
        sink.pending_space = false;
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
    // ASCII whitespace only: a no-break space joins its neighbours into one
    // unbreakable word.
    for word in normalized.split_ascii_whitespace() {
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
            sink.library.mark_last_block_paragraph_end();
            // Height-only change: the trailing paragraph gap moved without
            // block_count moving, so the page index is stale all the same.
            sink.pages_dirty = true;
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
        let _ = sink.library.push_line_block(
            line.as_str(),
            style,
            role,
            align,
            paragraph_end,
            sink.spine_index,
        );
    }
    sink.pages_dirty = true;
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

/// Collapse ASCII whitespace and drop invisible format characters. Entities
/// were already decoded by the XHTML parser; decoding again here turned
/// literal text such as `&lt;` (from `&amp;lt;`) into markup characters.
fn push_normalized<const N: usize>(text: &str, out: &mut String<N>) {
    let mut previous_space = true;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else if is_invisible_format_char(ch) {
            continue;
        } else if push_normalized_char(ch, out).is_err() {
            break;
        } else {
            previous_space = false;
        }
    }
}

/// Characters with no glyph and no width: soft hyphen, zero-width
/// space/joiners, word joiner, byte-order mark. Without this they render as
/// missing-glyph boxes in the middle of words.
fn is_invisible_format_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

fn push_normalized_char<const N: usize>(ch: char, out: &mut String<N>) -> Result<(), ()> {
    match ch {
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
        .find(|(_, ch)| !ch.is_ascii_whitespace())
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
