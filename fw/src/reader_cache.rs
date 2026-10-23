#[cfg(feature = "bench-serial")]
use crate::block_sink::build_profile;
pub(crate) use crate::block_sink::Preparation;
#[cfg(feature = "bench-serial")]
use crate::block_sink::{repair_profile, HYPHEN_LOOKUPS, HYPHEN_LOOKUP_MICROS};
use crate::block_sink::{LibraryBlockSink, LineInkCursor, LowerAscii, PendingImages, SpineRun};
use crate::book_images::{BookImages, ProbeStop};
use crate::display_flush::Epd;
use crate::image_decode::{DecodeOutcome, ImageScratch, ImageSession, PageImages, SessionStats};
use crate::reader_cache_files;
use crate::reader_cache_files::{BookIndexLoadResult, CacheLoadResult};
use crate::reader_layout;
use crate::reader_store::{
    source_hash, BookLoadStatus, ReaderStore, MAX_BOOK_SECTIONS, MAX_SEED_SECTIONS,
};
use crate::sd_session::{self, SdSessionError};
use display::font::FontStyle;
use embassy_time::Instant;
use embedded_sdmmc::{Directory, File, Mode, TimeSource};
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;
use heapless::String;
use proto::book::BookId;
use proto::epub::{
    parse_opf_streamed, strip_fragment, Epub3NavStreamParser, EpubTocSink, EpubZipOps,
    NcxStreamParser, ReadAt, StreamingXmlTokenizer, TocError, XhtmlBlockStreamParser, XhtmlError,
    ZipInflateScratch, ZipStream, MAX_ENTRY_NAME_BYTES,
};
use proto::hyphen::Hyphenation;
use proto::text::{TextAlign, TextRole};

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

    fn push_bodymatter(&mut self, href: &str) {
        if let Some(spine) = self
            .package
            .spine
            .iter()
            .position(|item| href_matches_spine(href, item.href.of(self.package.opf_text)))
        {
            self.library.set_text_start_spine(u16::try_from(spine).ok());
        }
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
    ) -> Self {
        Self {
            tail,
            header,
            name,
            compressed,
            container,
            opf,
            xhtml,
            zip_inflate: ZipInflateScratch::new(),
        }
    }
}

impl ReaderCacheScratch<'_> {
    /// Lends the scratch to an image pass, and a work list for it. A pass
    /// runs in an SD session of its own, never inside a build session, so
    /// all of it is idle: no spine streams (the compressed scratch, the
    /// XHTML window, the name scratch and the inflater), and no package is
    /// parsed (the container scratch, which holds the list). The tail, the
    /// header and the OPF arena stay unused: an image is read by the offset
    /// its index entry keeps, so no ZIP directory is opened.
    pub(crate) fn image_parts(&mut self) -> (ImageScratch<'_>, &mut [u8]) {
        let (input, stage) = self.compressed.split_at_mut(READER_COMPRESSED_SCRATCH / 2);
        (
            ImageScratch {
                input,
                stage,
                window: &mut self.xhtml[..],
                name: &mut self.name[..],
                inflate: &mut self.zip_inflate,
            },
            &mut self.container[..],
        )
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
    preparation: Preparation,
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
        && library.book_sections.is_empty()
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
            preparation,
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
    preparation: Preparation,
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
    let epub = EpubSource {
        name: open_name.as_str(),
        in_books_dir,
    };
    if try_load_v2_book_cache(
        root,
        cache_key.as_str(),
        source_identity,
        target_pages as u32,
        requested_spine,
        library,
        Instant::now(),
        "fast",
        epub,
        scratch,
        preparation,
    ) {
        return BookLoadStatus::Ready;
    }
    // Preparation follows. If the type settings or orientation changed since
    // the on-card pages were laid out, pin the requested page in them now,
    // before the rebuild replaces them. Once per request: later steps of the
    // same rebuild keep the pin rather than re-reading the index. An idle
    // step only appends, so its page keeps its number.
    let target_page = target_pages as u32;
    if preparation == Preparation::Request
        && requested_spine.is_none()
        && !library.anchors(source_identity, target_page)
    {
        let anchor = reader_cache_files::read_v2_page_anchor(
            root,
            cache_key.as_str(),
            source_identity,
            library,
            target_page,
        );
        library.set_page_anchor(Some(anchor));
    }

    let load_result = if in_books_dir {
        match root.open_dir("BOOKS") {
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
                    preparation,
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
        }
    } else {
        match root.open_file_in_dir(open_name.as_str(), Mode::ReadOnly) {
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
                preparation,
            )),
            Err(err) => {
                esp_println::println!("epub: open file failed: {:?}", err);
                set_preview_error(library, "FILE");
                None
            }
        }
    };
    if let (Preparation::Idle { .. }, Some(Err(err))) = (preparation, load_result) {
        // Nothing reached the index, so the card still holds the prefix as
        // it stood: read the page on screen back from it. A failed step
        // costs only the spine it was on, never the reader's page.
        if err == ReaderCacheError::Interrupted {
            esp_println::println!("epub: idle preparation gave way to a button press");
        } else {
            esp_println::println!("epub: idle preparation failed: {:?}", err);
        }
        library.begin_book_load();
        if try_load_v2_book_cache(
            root,
            cache_key.as_str(),
            source_identity,
            target_page,
            None,
            library,
            Instant::now(),
            "resume",
            epub,
            scratch,
            Preparation::Request,
        ) {
            return BookLoadStatus::Ready;
        }
        set_preview_error(library, "CACHE READ");
        return BookLoadStatus::Error;
    }
    status_for_load_result(load_result, library)
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

/// Which of an open book's images an idle image pass decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImagePass {
    /// Those on the pages around the one on screen.
    Window,
    /// The rest of the book's, nearest the reader first.
    Rest,
}

/// How an idle image pass ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImagePassEnd {
    /// A press stopped it.
    pub(crate) pressed: bool,
    /// It ran to its end rather than out of the session budget: the window
    /// through its list, the rest pass until nothing was left or a card
    /// fault stopped it.
    pub(crate) finished: bool,
    /// It decoded or failed an image.
    pub(crate) worked: bool,
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// A button press has landed since `crate::BUTTON_PRESSES` read `presses`.
fn pressed_since(presses: u32) -> bool {
    crate::BUTTON_PRESSES.load(core::sync::atomic::Ordering::Relaxed) != presses
}

/// Decodes the open book's images while the reader is idle, for the
/// orientation the page on screen is laid out in: one pass, in an SD
/// session of its own, until it runs out of work, out of the session
/// budget, or gives way to a press. Opens the book's EPUB only once an
/// image needs reading, so a pass that finds nothing reads only the index
/// (and, for the window, the section files of pages the store does not
/// hold). What it finds settled goes into the store's note: the window's
/// pages, or, when the rest pass finds nothing left, the whole book. Callers
/// ask only for books with images.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_images_while_idle(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    page: u32,
    scratch: &mut ReaderCacheScratch<'_>,
    pass: ImagePass,
    presses: u32,
) -> ImagePassEnd {
    let started = Instant::now();
    let gives_way = || pressed_since(presses);
    let out_of_time = || started.elapsed().as_millis() >= SESSION_BUDGET_MS;
    let (mut parts, list) = scratch.image_parts();
    let store: &ReaderStore = library;
    // What the pass can vouch for: the window's pages, or the whole book.
    let mut settled = None;
    let mut out_of_budget = false;
    let (stats, images) = run_image_session(epd, sd_cs, store, index, |root, book| {
        let dir = crate::book_images::book_cache_dir(root, book.key.as_str())?;
        let images = crate::book_images::open_existing_book_images(&dir, book.source)?;
        let books = core::cell::OnceCell::new();
        let mut listed = 0;
        let mut session = ImageSession::new(
            &dir,
            images,
            epub_opener(root, &books, book.epub()),
            store.portrait(),
            now_ms,
        );
        match pass {
            ImagePass::Window => {
                let mut window = PageImages::new(list);
                let pages = crate::image_decode::list_window_images(
                    store,
                    index,
                    page,
                    &dir,
                    book.source,
                    &mut window,
                );
                listed = window.len();
                let mut whole = true;
                for image in (0..window.len()).filter_map(|at| window.get(at)) {
                    if gives_way() || out_of_time() {
                        whole = false;
                        out_of_budget = !gives_way();
                        break;
                    }
                    if matches!(
                        session.decode_page_image(image, &mut parts, &gives_way),
                        Some(DecodeOutcome::Cancelled | DecodeOutcome::Retry)
                    ) {
                        whole = false;
                        break;
                    }
                }
                if whole {
                    settled = pages;
                }
            }
            ImagePass::Rest => {
                let reader = crate::image_decode::reader_place(store, page, session.images());
                if session.decode_nearest(reader, list, &mut parts, &gives_way, &out_of_time) {
                    settled = Some((0, u32::MAX));
                } else {
                    out_of_budget = out_of_time();
                }
            }
        }
        Some((session.stats, listed))
    })
    .unwrap_or_default();
    let pressed = gives_way();
    if stats.forgot_decodes() {
        library.forget_settled_images();
    }
    match settled {
        Some((0, u32::MAX)) if !stats.forgot_decodes() => library.note_book_images_settled(),
        Some((from, to)) if !stats.forgot_decodes() => library.note_settled_pages(from, to),
        _ => {}
    }
    log_image_session(
        match pass {
            ImagePass::Window => "window",
            // Not "rest": a four-byte string constant would sit in RAM on
            // the ESP32-C3, as eight- and sixteen-byte ones do.
            ImagePass::Rest => "remaining",
        },
        images,
        stats,
        started,
        pressed,
    );
    ImagePassEnd {
        pressed,
        finished: !out_of_budget && !pressed,
        worked: stats.decoded > 0 || stats.failed > 0,
    }
}

/// Decodes the images the page about to be shown draws, for its
/// orientation, before the request is answered, so the page never shows
/// a placeholder that a picture would later replace. The page is resident
/// by now; one without images, in a book without any, or that the store's
/// note says is settled, costs nothing (no card access); one found settled
/// is noted. False when the wait is abandoned, the image in hand dropped,
/// so the press is handled at once: a press since `presses` was read, or a
/// newer request than `request_id`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_page_images(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    index: usize,
    page: u32,
    scratch: &mut ReaderCacheScratch<'_>,
    presses: u32,
    request_id: u32,
) -> bool {
    if !library.book_has_images()
        || !library.covers_global_page(index, page)
        || library.images_settled(page)
    {
        return true;
    }
    let (mut parts, list) = scratch.image_parts();
    let mut shown = PageImages::new(list);
    let whole = crate::image_decode::resident_page_images(library, page, &mut shown);
    if shown.is_empty() && whole {
        return true;
    }
    let started = Instant::now();
    let gives_way = || {
        pressed_since(presses)
            || crate::LATEST_READER_REQUEST_ID.load(core::sync::atomic::Ordering::Relaxed)
                != request_id
    };
    if gives_way() {
        return false;
    }
    let store: &ReaderStore = library;
    let mut settled = whole;
    let (stats, images) = run_image_session(epd, sd_cs, store, index, |root, book| {
        let dir = crate::book_images::book_cache_dir(root, book.key.as_str())?;
        let images = crate::book_images::open_existing_book_images(&dir, book.source)?;
        let books = core::cell::OnceCell::new();
        let mut session = ImageSession::new(
            &dir,
            images,
            epub_opener(root, &books, book.epub()),
            store.portrait(),
            now_ms,
        );
        for image in (0..shown.len()).filter_map(|at| shown.get(at)) {
            if gives_way() {
                settled = false;
                break;
            }
            if matches!(
                session.decode_page_image(image, &mut parts, &gives_way),
                Some(DecodeOutcome::Cancelled | DecodeOutcome::Retry)
            ) {
                settled = false;
                if gives_way() {
                    break;
                }
            }
        }
        Some((session.stats, shown.len()))
    })
    // No index to check against: nothing is vouched for.
    .filter(|(stats, _)| stats.checked > 0 || shown.is_empty())
    .unwrap_or_else(|| {
        settled = false;
        (SessionStats::default(), 0)
    });
    let pressed = gives_way();
    if stats.forgot_decodes() {
        library.forget_settled_images();
    } else if settled && !pressed {
        library.note_settled_pages(page, page + 1);
    }
    log_image_session("request", images, stats, started, pressed);
    !pressed
}

/// The open book's files an image session reads: its cache key, its
/// source identity, and where its EPUB is.
struct BookFiles<'a> {
    key: String<{ proto::cache::CACHE_KEY_BYTES }>,
    source: (u32, u32),
    name: &'a str,
    in_books_dir: bool,
}

impl BookFiles<'_> {
    fn epub(&self) -> EpubSource<'_> {
        EpubSource {
            name: self.name,
            in_books_dir: self.in_books_dir,
        }
    }
}

/// Runs `pass` in an SD session for the book in catalog entry `index`,
/// and returns what it reports: its stats and how many images it listed.
/// `None` when the session, the cache directory or the index did not open.
fn run_image_session<F>(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &ReaderStore,
    index: usize,
    pass: F,
) -> Option<(SessionStats, usize)>
where
    F: for<'r> FnOnce(
        &crate::sd_session::SdRoot<'r>,
        &BookFiles<'_>,
    ) -> Option<(SessionStats, usize)>,
{
    let entry = library.catalog_entry(index)?;
    let book = BookFiles {
        key: proto::cache::cache_key_for(entry.display_name.as_str(), entry.byte_size),
        source: (entry.source_hash, entry.byte_size),
        name: entry.open_name.as_str(),
        in_books_dir: entry.in_books_dir,
    };
    sd_session::with_root(epd, sd_cs, |root| pass(root, &book))
        .ok()
        .flatten()
}

fn log_image_session(
    pass: &str,
    images: usize,
    stats: SessionStats,
    started: Instant,
    pressed: bool,
) {
    esp_println::println!(
        "bench: image_session pass={} images={} checked={} decoded={} failed={} cancelled={} retry={} elapsed_ms={} pressed={} t_ms={}",
        pass,
        images,
        stats.checked,
        stats.decoded,
        stats.failed,
        stats.cancelled,
        stats.retry,
        started.elapsed().as_millis(),
        pressed,
        Instant::now().as_millis(),
    );
}

/// Opens the book's EPUB the first time it is called, for reads at
/// offsets: no ZIP directory is read. An EPUB in `/BOOKS` keeps that
/// directory open in `books` for as long as the file is.
fn epub_opener<'g, 'f: 'g, D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &'f Directory<'_, D, T, ND, NF, NV>,
    books: &'g core::cell::OnceCell<Directory<'f, D, T, ND, NF, NV>>,
    epub: EpubSource<'g>,
) -> impl FnMut() -> Option<SdFileReadAt<'g, D, T, ND, NF, NV>> + use<'g, 'f, D, T, ND, NF, NV>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    move || {
        let dir = if epub.in_books_dir {
            if books.get().is_none() {
                let _ = books.set(root.open_dir("BOOKS").ok()?);
            }
            books.get()?
        } else {
            root
        };
        let file = dir.open_file_in_dir(epub.name, Mode::ReadOnly).ok()?;
        let len = file.length();
        Some(SdFileReadAt {
            file,
            len,
            read_ops: 0,
            read_bytes: 0,
        })
    }
}

/// Where a catalog entry's EPUB file is on the card.
#[derive(Clone, Copy)]
struct EpubSource<'a> {
    /// The FAT name it opens by.
    name: &'a str,
    /// In `/BOOKS`, else the card root.
    in_books_dir: bool,
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
    epub: EpubSource<'_>,
    scratch: &mut ReaderCacheScratch<'_>,
    preparation: Preparation,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut result =
        reader_cache_files::load_v2_book_index(root, cache_key, source_identity, library);
    if let BookIndexLoadResult::BeforeImages { spacing_changed } = result {
        // Laid out before image blocks existed. The ZIP's central directory
        // says whether that matters: once per book, as whatever follows
        // writes the index as the current version. A lone cover does not.
        let checked = Instant::now();
        let has_images = epub_has_images_beyond_a_cover(root, epub, scratch);
        esp_println::println!(
            "epub: v27 book index; images={:?} checked in {} ms",
            has_images,
            checked.elapsed().as_millis()
        );
        result = reader_cache_files::settle_index_before_images(
            root,
            cache_key,
            source_identity,
            library,
            has_images,
            spacing_changed,
            requested_global_page,
            requested_spine,
        );
    }
    let reflow_scratch = &mut scratch.xhtml[..];
    if result == BookIndexLoadResult::SpacingMismatch {
        // The resident sections are still the old spacing's pages.
        if requested_spine.is_none() {
            library.anchor_resident_page(source_identity, requested_global_page);
        }
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
            library.book_sections.len(),
            cache_key
        );
        result = BookIndexLoadResult::Hit;
    }
    match result {
        BookIndexLoadResult::Hit => {
            // A durable prefix is where preparation resumes. It answers a
            // request whose page it already holds; anything else, and every
            // idle-time step, goes on to append the next spine.
            let requested = library.requested_page(requested_global_page, requested_spine);
            if library.book_next_spine().is_some()
                && (preparation != Preparation::Request || requested.is_none())
            {
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
            let Some(requested_global_page) = requested else {
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
        BookIndexLoadResult::Invalid
        | BookIndexLoadResult::SpacingMismatch
        | BookIndexLoadResult::BeforeImages { .. } => {
            esp_println::println!("epub: v2 {label} book index invalid");
            false
        }
        BookIndexLoadResult::Miss => {
            esp_println::println!("epub: v2 {label} book index miss");
            false
        }
    }
}

/// Whether the book's ZIP has image entries beyond a lone cover
/// (`book_images::book_has_images_beyond_a_cover`), from its central
/// directory alone: the open reads the end record and the directory (or
/// indexes it), and the check walks it once more, from RAM when the open
/// cached it and through the idle compressed scratch otherwise. No image
/// is read. None when the book cannot be opened; a directory damaged past
/// the open counts as no images, as it does for a build.
#[inline(never)]
fn epub_has_images_beyond_a_cover<D, T, const ND: usize, const NF: usize, const NV: usize>(
    root: &Directory<'_, D, T, ND, NF, NV>,
    epub: EpubSource<'_>,
    scratch: &mut ReaderCacheScratch<'_>,
) -> Option<bool>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let books;
    let dir = if epub.in_books_dir {
        books = root.open_dir("BOOKS").ok()?;
        &books
    } else {
        root
    };
    let file = dir.open_file_in_dir(epub.name, Mode::ReadOnly).ok()?;
    let len = file.length();
    let reader = SdFileReadAt {
        file,
        len,
        read_ops: 0,
        read_bytes: 0,
    };
    let mut zip = ZipStream::new(reader, scratch.tail).ok()?;
    Some(crate::book_images::book_has_images_beyond_a_cover(
        &mut zip,
        scratch.compressed,
    ))
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
    result: Option<Result<BookLoadStatus, ReaderCacheError>>,
    library: &mut ReaderStore,
) -> BookLoadStatus {
    match result {
        Some(Ok(status)) => status,
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
        ReaderCacheError::Interrupted => "INTERRUPTED",
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
    /// An idle-time step gave way to a button press before committing.
    Interrupted,
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
    preparation: Preparation,
) -> Result<BookLoadStatus, ReaderCacheError>
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
        preparation,
        ZipBuildScratch {
            header: scratch.header,
            name: scratch.name,
            compressed: scratch.compressed,
            container: scratch.container,
            opf: scratch.opf,
            xhtml: scratch.xhtml,
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
    preparation: Preparation,
    scratch: ZipBuildScratch<'_>,
) -> Result<BookLoadStatus, ReaderCacheError>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // An index from this generation is resident: a resumed build. Its
    // prefix cursor is where the prefix goes on; with nothing laid out yet
    // the build starts at the first spine.
    let resume_next = library.book_next_spine();
    let resuming = !library.book_sections.is_empty() || !library.seed.sections.is_empty();
    let section_base = if !library.book_sections.is_empty() {
        (library.book_sections.base() / MAX_BOOK_SECTIONS as u16) * MAX_BOOK_SECTIONS as u16
    } else if let Some(first) = library.seed.sections.iter().next() {
        let seed = first.section - crate::reader_store::SEED_SECTION_BASE;
        (seed / MAX_SEED_SECTIONS as u16) * MAX_BOOK_SECTIONS as u16
    } else {
        reader_cache_files::prepare_book_replacement(root, cache_key)
            .ok_or(ReaderCacheError::CacheWrite)?
    };
    if !resuming {
        // A fresh build numbers the pages anew, so what is noted of their
        // images goes.
        library.forget_settled_images();
    }
    let seed_base = crate::reader_store::seed_section_base(section_base);
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
    // The rest of the container scratch is idle once the package is found:
    // the block sink holds images there while a spine is laid out.
    let (container_text, pending_room) = scratch
        .container
        .split_at_mut(container_len.min(READER_CONTAINER_SCRATCH));
    let container_xml = core::str::from_utf8(container_text).map_err(|_| ReaderCacheError::Utf8)?;
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

    if preparation.gives_way() {
        return Err(ReaderCacheError::Interrupted);
    }
    library.set_book_labels(package.meta.title, package.meta.author);
    library.clear_cover();
    if !resuming {
        // Found afresh below: the OPF guide's text reference, else the nav's
        // bodymatter landmark (recorded while the TOC streams), else a guess.
        // A resumed build keeps the start its prefix index recorded.
        library.set_text_start_spine(None);
    }
    if resuming {
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
    if !resuming {
        library.book_sections.clear();
        library.seed.clear();
    }
    // A spine clipped at MAX_SPINE_ITEMS means the tail chapters were dropped at
    // parse, so the book is partial even if every kept section caches cleanly.
    let mut book_partial = package.spine_truncated || (resuming && library.book_cache_partial);
    let generate_toc_from_headings = if resuming {
        library.book_generated_toc()
    } else {
        library.toc_count() == 0
    };
    library.set_book_generated_toc(generate_toc_from_headings);
    if !resuming {
        let guide_start = package.text_reference_href.and_then(|href| {
            package
                .spine
                .iter()
                .position(|item| href_matches_spine(href, item.href.of(package.opf_text)))
        });
        let text_start = guide_start
            .and_then(|spine| u16::try_from(spine).ok())
            .or(library.text_start_spine())
            .or_else(|| inferred_text_start_spine(&package));
        library.set_text_start_spine(text_start.filter(|spine| *spine > 0));
    }
    // Front matter is part of the book: a first open lands at the text start
    // (see ReaderStore::requested_page), and paging back or the Contents
    // still reach the cover, title, and foreword pages.
    let mut prefix_next = next_eligible_spine(&package, resume_next.map_or(0, usize::from));
    // A book with images keeps an index of them beside its book index,
    // built once from the central directory through the compressed scratch
    // and the XHTML window, both idle until the first spine. A fresh build
    // learns whether the book has any from the ZIP's open (walking only a
    // directory too big for its index pass), and the book index keeps the
    // answer for the sessions that resume it. A book without them never
    // touches the index.
    let has_images = if resuming {
        library.book_has_images()
    } else {
        let found = crate::book_images::book_has_images(&mut zip, &mut scratch.compressed[..]);
        library.set_book_has_images(found);
        found
    };
    let images_dir;
    let mut rebuilt = false;
    let images = if has_images {
        images_dir = crate::book_images::book_cache_dir(root, cache_key);
        images_dir.as_ref().and_then(|dir| {
            crate::book_images::open_book_images(
                dir,
                source_identity,
                &mut zip,
                &mut scratch.compressed[..],
                &mut scratch.xhtml[..],
                &mut rebuilt,
            )
        })
    } else {
        None
    };
    if rebuilt {
        // A new index: every image is undecoded again.
        library.forget_settled_images();
    }
    let mut spine_scratch = SpineScratch {
        header: &mut *scratch.header,
        name: &mut *scratch.name,
        compressed: &mut *scratch.compressed,
        xhtml: &mut *scratch.xhtml,
        zip_inflate: &mut *scratch.zip_inflate,
        pending: pending_room,
    };
    // One session lays out spines until the request is answered, a button
    // is pressed, the budget is spent, or the book is done, committing the
    // index after each: no ZIP, OPF, or index reload between spines, and
    // stopping anywhere loses at most the spine in hand.
    let presses = crate::BUTTON_PRESSES.load(core::sync::atomic::Ordering::Relaxed);
    let mut last_commit = Instant::now();
    let mut spines_laid = 0usize;
    let mut saw_spine = resuming;
    while let Some((spine_index, run)) = session_target(
        &mut zip,
        &package,
        opf_path,
        library,
        preparation,
        requested_global_page,
        requested_spine,
        prefix_next,
        generate_toc_from_headings,
        &mut spine_scratch,
    ) {
        saw_spine = true;
        let run_base = match run {
            SpineRun::Prefix => section_base,
            SpineRun::Seed => seed_base,
        };
        if run == SpineRun::Seed && library.seed.first_spine.is_none() {
            library.seed.first_spine = Some(spine_index as u16);
            esp_println::println!("epub: laying out spine {} ahead of the prefix", spine_index);
        }
        let seed_len = library.seed.sections.len();
        // A spine that meets images no probe has read is laid out twice; see
        // `lay_out_spine`. Nothing commits in between: the book index is
        // written below, after the second pass.
        let mut first_pass = true;
        let outcome = loop {
            let laid = lay_out_spine(
                &mut zip,
                root,
                source_identity,
                &package,
                opf_path,
                spine_index,
                run,
                run_base,
                library,
                font_metrics,
                preparation,
                generate_toc_from_headings && run == SpineRun::Prefix,
                &mut book_partial,
                &mut section_write_micros,
                images.as_ref(),
                first_pass,
                &mut spine_scratch,
            )?;
            match laid {
                SpinePass::Laid(outcome) => break outcome,
                SpinePass::Again if first_pass => first_pass = false,
                // A second pass never waits for a probe. Were one to, its
                // sections are already taken back: fail the step, which
                // commits nothing, rather than move past the spine.
                SpinePass::Again => return Err(ReaderCacheError::CacheWrite),
            }
        };
        spines_laid += 1;
        let after = next_eligible_spine(&package, spine_index + 1);
        match run {
            SpineRun::Prefix => prefix_next = after,
            SpineRun::Seed if outcome == SpineOutcome::RunFull => {
                // A run that cannot hold the spine stops growing (even an
                // empty one: the spine would never fit); the prefix will
                // reach the rest.
                library.seed.sections.truncate(seed_len);
                library.seed.capped = true;
                library.seed.next_spine = Some(spine_index as u16);
            }
            SpineRun::Seed => library.seed.next_spine = after.map(|spine| spine as u16),
        }
        note_book_progress(library, prefix_next, &mut book_partial);
        let stop = prefix_next.is_none()
            || match preparation {
                Preparation::Request => {
                    library
                        .requested_page(requested_global_page, requested_spine)
                        .is_some()
                        || crate::BUTTON_PRESSES.load(core::sync::atomic::Ordering::Relaxed)
                            != presses
                }
                Preparation::Idle { .. } => preparation.gives_way(),
            }
            || spine_started.elapsed().as_millis() >= SESSION_BUDGET_MS;
        // Each index write costs a few dozen block writes, so a session
        // writes it as it ends and otherwise only every so often: a press
        // mid-spine loses at most the spines since.
        if stop || last_commit.elapsed().as_millis() >= COMMIT_INTERVAL_MS {
            write_book_progress(
                root,
                cache_key,
                source_identity,
                library,
                book_partial,
                generate_toc_from_headings,
                &mut *spine_scratch.xhtml,
            )?;
            last_commit = Instant::now();
        }
        if stop {
            break;
        }
    }
    if library.book_sections.is_full() && prefix_next.is_some() {
        book_partial = true;
    }

    let resolved_page = library.requested_page(requested_global_page, requested_spine);
    // An unanswered request still leaves a real committed prefix resident
    // while the task schedules the next session.
    let loaded_page = resolved_page.or_else(|| (!library.book_sections.is_empty()).then_some(0));
    let Some(loaded_page) = loaded_page else {
        reader_layout::rebuild_toc_page_targets(library);
        return if prefix_next.is_some() || !library.seed.sections.is_empty() {
            Ok(BookLoadStatus::Loading)
        } else if saw_spine {
            Err(ReaderCacheError::NoBodyText)
        } else {
            Err(ReaderCacheError::MissingSpine)
        };
    };
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
        "epub: book cache session ready after {} ms (pages={} sections={} seed_sections={} partial={} cover={:?} key {} next_spine={:?})",
        open_started.elapsed().as_millis(),
        library.book_total_pages,
        library.book_sections.len(),
        library.seed.sections.len(),
        book_partial,
        cover,
        cache_key,
        library.book_next_spine()
    );
    let io = crate::sd_session::sd_stats::snapshot().since(io_start);
    esp_println::println!(
        "bench: storage_build elapsed_ms={} spine_ms={} write_ms={} spines={} sections={} seed_sections={} pages={} rd_calls={} rd_blocks={} wr_calls={} wr_blocks={} key={} complete={}",
        open_started.elapsed().as_millis(),
        spine_started.elapsed().as_millis(),
        section_write_micros / 1000,
        spines_laid,
        library.book_sections.len(),
        library.seed.sections.len(),
        library.book_total_pages,
        io.read_calls,
        io.read_blocks,
        io.write_calls,
        io.write_blocks,
        cache_key,
        library.book_next_spine().is_none()
    );
    #[cfg(feature = "bench-serial")]
    esp_println::println!(
        "bench: hyphenation lookups={} lookup_ms={} hits={}",
        HYPHEN_LOOKUPS.swap(0, portable_atomic::Ordering::Relaxed),
        HYPHEN_LOOKUP_MICROS.swap(0, portable_atomic::Ordering::Relaxed) / 1000,
        repair_profile::LOOKUP_HITS.swap(0, portable_atomic::Ordering::Relaxed),
    );
    #[cfg(feature = "bench-serial")]
    {
        use portable_atomic::Ordering;
        use repair_profile::*;
        esp_println::println!(
            "bench: line_repair repairs={} repair_ms={} pieces={} piece_ms={} candidates={} relax_ms={} search_lookups={} search_lookup_ms={} keep_ms={} kept_measure_ms={}",
            REPAIRS.swap(0, Ordering::Relaxed),
            REPAIR_MICROS.swap(0, Ordering::Relaxed) / 1000,
            PIECES.swap(0, Ordering::Relaxed),
            PIECE_MICROS.swap(0, Ordering::Relaxed) / 1000,
            CANDIDATES.swap(0, Ordering::Relaxed),
            RELAX_MICROS.swap(0, Ordering::Relaxed) / 1000,
            SEARCH_LOOKUPS.swap(0, Ordering::Relaxed),
            SEARCH_LOOKUP_MICROS.swap(0, Ordering::Relaxed) / 1000,
            KEEP_MICROS.swap(0, Ordering::Relaxed) / 1000,
            MEASURE_MICROS.swap(0, Ordering::Relaxed) / 1000,
        );
    }
    #[cfg(feature = "bench-serial")]
    esp_println::println!(
        "bench: build_profile spine_reads_ms={} feeds_ms={} paragraphs_ms={} lines_ms={} pages_ms={} commits_ms={} sd_read_ms={} sd_write_ms={} section_write_reads={}",
        build_profile::take_ms(&build_profile::SPINE_READS),
        build_profile::take_ms(&build_profile::FEEDS),
        build_profile::take_ms(&build_profile::PARAGRAPHS),
        build_profile::take_ms(&build_profile::LINES),
        build_profile::take_ms(&build_profile::PAGES),
        build_profile::take_ms(&build_profile::COMMITS),
        crate::sd_session::sd_stats::take(&crate::sd_session::sd_stats::READ_MICROS) / 1000,
        crate::sd_session::sd_stats::take(&crate::sd_session::sd_stats::WRITE_MICROS) / 1000,
        build_profile::SECTION_WRITE_READS.swap(0, portable_atomic::Ordering::Relaxed),
    );
    Ok(if resolved_page.is_some() {
        BookLoadStatus::Ready
    } else {
        BookLoadStatus::Loading
    })
}

/// How long one build session lays out spines before handing the task
/// back. The session commits as it ends, so ending it loses nothing; the
/// budget bounds how long progress writes and sleep wait.
const SESSION_BUDGET_MS: u64 = 4000;
/// Within a session, how often the book index is written.
const COMMIT_INTERVAL_MS: u64 = 1500;

/// A seed run is started only past at least this much XHTML: below it,
/// laying out the gap in order costs about what the seed would save.
const SEED_MIN_GAP_BYTES: u32 = 64 * 1024;
/// Nor on a spine bigger than this (some 800 pages), which could outgrow
/// the run's table and be laid out for nothing.
const SEED_MAX_SPINE_BYTES: u32 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpineOutcome {
    Laid,
    /// The spine names no member to read.
    Skipped,
    /// The seed run filled before the spine was through.
    RunFull,
}

struct SpineScratch<'a> {
    header: &'a mut [u8; READER_HEADER_SCRATCH],
    name: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    compressed: &'a mut [u8; READER_COMPRESSED_SCRATCH],
    xhtml: &'a mut [u8; READER_XHTML_SCRATCH],
    zip_inflate: &'a mut ZipInflateScratch,
    /// Where the block sink holds the images met at a paragraph's start
    /// until it knows whether text follows: the container scratch past
    /// `container.xml`, idle while spines are laid out.
    pending: &'a mut [u8],
}

/// First spine at or after `from` that holds content to lay out.
fn next_eligible_spine(package: &proto::epub::EpubPackage<'_>, from: usize) -> Option<usize> {
    package
        .spine
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, item)| !item.href.is_empty() && !spine_item_is_navigation(item, package))
        .map(|(index, _)| index)
}

/// The spine a session lays out next, and into which run, or None when it
/// is done. Idle sessions extend the prefix (re-laying any seeded spines
/// it reaches, which come out the same). A request extends the prefix too,
/// unless it names a spine well ahead of it: then that spine is laid out
/// first, into a seed run with provisionally numbered pages, and the run
/// grows toward the request from there.
#[allow(clippy::too_many_arguments)]
fn session_target<Z: EpubZipOps>(
    zip: &mut Z,
    package: &proto::epub::EpubPackage<'_>,
    opf_path: &str,
    library: &ReaderStore,
    preparation: Preparation,
    requested_page: u32,
    requested_spine: Option<u16>,
    prefix_next: Option<usize>,
    generated_toc: bool,
    scratch: &mut SpineScratch<'_>,
) -> Option<(usize, SpineRun)> {
    let prefix = prefix_next?;
    if preparation != Preparation::Request {
        return Some((prefix, SpineRun::Prefix));
    }
    let Some(wanted) = library.request_spine(requested_page, requested_spine) else {
        return Some((prefix, SpineRun::Prefix));
    };
    let Some(wanted) = next_eligible_spine(package, usize::from(wanted)) else {
        return Some((prefix, SpineRun::Prefix));
    };
    if wanted <= prefix {
        return Some((prefix, SpineRun::Prefix));
    }
    let seed = &library.seed;
    match seed.first_spine {
        // Grow the run toward the request; one behind the run waits for
        // the prefix, as a second run would renumber the first's pages.
        Some(first) if wanted >= usize::from(first) && !seed.capped => seed
            .next_spine
            .and_then(|next| next_eligible_spine(package, usize::from(next)))
            .map(|next| (next, SpineRun::Seed))
            .or(Some((prefix, SpineRun::Prefix))),
        Some(_) => Some((prefix, SpineRun::Prefix)),
        None if !generated_toc
            && !zip.is_forward_only()
            && library.book_total_pages < crate::reader_store::PROVISIONAL_PAGE_FLOOR / 2
            && gap_worth_seeding(zip, package, opf_path, prefix, wanted, scratch) =>
        {
            Some((wanted, SpineRun::Seed))
        }
        None => Some((prefix, SpineRun::Prefix)),
    }
}

/// Whether seeding spine `to` is worth it: the spines in `from..to` hold at
/// least `SEED_MIN_GAP_BYTES` of XHTML and `to` itself fits a run. Reads
/// only their ZIP sizes.
fn gap_worth_seeding<Z: EpubZipOps>(
    zip: &mut Z,
    package: &proto::epub::EpubPackage<'_>,
    opf_path: &str,
    from: usize,
    to: usize,
    scratch: &mut SpineScratch<'_>,
) -> bool {
    let mut path = String::<MAX_ENTRY_NAME_BYTES>::new();
    let mut size = |zip: &mut Z, item: proto::epub::SpineItem| -> Option<u32> {
        if item.href.is_empty() || spine_item_is_navigation(&item, package) {
            return None;
        }
        resolve_epub_href(opf_path, item.href.of(package.opf_text), &mut path).ok()?;
        zip.find_entry(&path, scratch.header, scratch.name)
            .ok()
            .map(|entry| entry.uncompressed_size)
    };
    if package
        .spine
        .get(to)
        .and_then(|item| size(zip, item))
        .is_none_or(|bytes| bytes > SEED_MAX_SPINE_BYTES)
    {
        return false;
    }
    let mut bytes = 0u32;
    for item in package.spine.iter().take(to).skip(from) {
        bytes = bytes.saturating_add(size(zip, item).unwrap_or(0));
        if bytes >= SEED_MIN_GAP_BYTES {
            return true;
        }
    }
    false
}

/// Bring the store's book state up to what the session has laid out: the
/// prefix's cursor and page total, and a full table ending it partial.
fn note_book_progress(
    library: &mut ReaderStore,
    prefix_next: Option<usize>,
    book_partial: &mut bool,
) {
    let mut next_spine = prefix_next.map(|spine| spine.min(u16::MAX as usize) as u16);
    if library.book_sections.is_full() && next_spine.is_some() {
        *book_partial = true;
        next_spine = None;
    }
    let total_pages = library.book_sections.end_page();
    library.commit_book_index(total_pages, *book_partial);
    library.set_book_next_spine(next_spine);
    // The spine may have met images no decode has reached.
    library.note_book_grew();
    // Chapter starts move with every spine laid out, seeded ones included
    // (which leave the prefix cursor where it was): reread the map.
    library.chapter_page_count = 0;
}

/// Write the book index for what the store holds: the prefix (with its
/// cursor) and any seed run. Nothing before the first section: the cursor
/// rides along in RAM until then.
fn write_book_progress<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    cache_key: &str,
    source_identity: (u32, u32),
    library: &ReaderStore,
    book_partial: bool,
    generated_toc: bool,
    scratch: &mut [u8],
) -> Result<(), ReaderCacheError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if library.book_sections.is_empty() && library.seed.sections.is_empty() {
        return Ok(());
    }
    #[cfg(feature = "bench-serial")]
    let _span = build_profile::Span::new(&build_profile::COMMITS);
    if !reader_cache_files::write_v2_book_index(
        root,
        cache_key,
        source_identity,
        library.book_sections.end_page(),
        &library.book_sections,
        library,
        book_partial,
    ) {
        return Err(ReaderCacheError::CacheWrite);
    }
    if generated_toc && !write_generated_toc(root, cache_key, source_identity, library, scratch) {
        return Err(ReaderCacheError::CacheWrite);
    }
    Ok(())
}

/// Lay out one spine into `run`'s table, writing its sections as they fill.
///
/// A spine whose first pass meets images no probe has read is laid out
/// twice: the first pass marks them in the image index and stops laying
/// out at the first of them, then probes them, and the second pass lays
/// the whole spine out with their sizes. Nothing commits between the
/// passes (the session writes the book index only after the second), and
/// the first is taken back before the second (`take_back_first_pass`): its
/// sections leave the run's table, and its generated TOC record goes. Its
/// section files are a prefix of what the second pass writes under the
/// same ids, since both lay the spine out identically up to the first
/// unprobed image, so the second overwrites every one of them.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn lay_out_spine<Z, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    zip: &mut Z,
    root: &Directory<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    source_identity: (u32, u32),
    package: &proto::epub::EpubPackage<'_>,
    opf_path: &str,
    spine_index: usize,
    run: SpineRun,
    section_base: u16,
    library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    preparation: Preparation,
    generate_toc_from_headings: bool,
    book_partial: &mut bool,
    write_micros: &mut u64,
    images: Option<&BookImages<'_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    first_pass: bool,
    scratch: &mut SpineScratch<'_>,
) -> Result<SpinePass, ReaderCacheError>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some(spine) = package.spine.get(spine_index) else {
        return Ok(SpinePass::Laid(SpineOutcome::Skipped));
    };
    let cache_key = library.cache_key.clone();
    library.clear_lines();
    let mut xhtml_path = String::<MAX_ENTRY_NAME_BYTES>::new();
    resolve_epub_href(opf_path, spine.href.of(package.opf_text), &mut xhtml_path)?;
    esp_println::println!("epub: find spine {}", xhtml_path.as_str());
    let xhtml_entry = match zip.find_entry(&xhtml_path, scratch.header, scratch.name) {
        Ok(entry) => entry,
        Err(proto::epub::ZipError::Io) => return Err(proto::epub::ZipError::Io.into()),
        Err(err) => {
            // A dangling spine reference names no content to lose; skip it
            // (a retryable storage fault returned above).
            esp_println::println!(
                "epub: spine {} not found ({:?}); skipping it",
                xhtml_path.as_str(),
                err
            );
            return Ok(SpinePass::Laid(SpineOutcome::Skipped));
        }
    };
    esp_println::println!(
        "epub: spine {} compressed={} uncompressed={}",
        xhtml_path.as_str(),
        xhtml_entry.compressed_size,
        xhtml_entry.uncompressed_size
    );
    let visible_page_capacity = library.page_capacity().max(1);
    reader_cache_files::with_v2_sections_dir(root, cache_key.as_str(), |sections_dir| {
        // The XHTML window only stages inflated chunks for the parser (the
        // inflater keeps its own history), so its tail is idle while the
        // spine streams: the line builder keeps its window of words there.
        let (xhtml_window, line_window) = scratch
            .xhtml
            .split_at_mut(READER_XHTML_SCRATCH - crate::line_window::LINE_WINDOW_BYTES);
        let type_settings = library.type_settings();
        let mut sink = LibraryBlockSink {
            library: &mut *library,
            root,
            sections_dir,
            source_identity,
            section_base,
            run,
            run_full: false,
            font_metrics: &mut *font_metrics,
            book_partial: &mut *book_partial,
            write_micros: &mut *write_micros,
            spine_index: spine_index.min(u16::MAX as usize) as u16,
            line: String::new(),
            line_ink: LineInkCursor::new(type_settings, FontStyle::Regular),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
            hyphenation: package
                .language
                .map(Hyphenation::for_language)
                .unwrap_or_default(),
            hyphen_run: 0,
            line_hyphenated: false,
            stopped: false,
            preparation,
            interrupted: false,
            target_pages: visible_page_capacity,
            generate_toc_from_headings,
            generated_toc_for_spine: false,
            pages_dirty: true,
            write_failed: false,
            images,
            chapter: xhtml_path.as_str(),
            image_path: &mut *scratch.name,
            first_pass,
            unprobed: None,
            images_met: 0,
            pending: PendingImages::new(&mut *scratch.pending),
            paragraph_has_text: false,
            line_image_height: 0,
            line_unindented: false,
            window: crate::line_window::LineWindow::new(line_window),
        };
        // Stream the whole member through the resumable block parser in
        // bounded windows: spine XHTML of any size decodes completely, with
        // no 24 KB prefix truncation. The parser's in-body assumption is
        // sniffed from the first decoded window, mirroring the
        // whole-document contains() check.
        let mut tokenizer = StreamingXmlTokenizer::new();
        let mut parser: Option<XhtmlBlockStreamParser> = None;
        let mut parse_error: Option<XhtmlError> = None;
        #[cfg(feature = "bench-serial")]
        let spine_span = build_profile::Span::new(&build_profile::SPINE_READS);
        let read_result = zip.read_entry_to_sink(
            xhtml_entry,
            &mut *scratch.compressed,
            xhtml_window,
            &mut *scratch.zip_inflate,
            &mut |chunk| {
                #[cfg(feature = "bench-serial")]
                let _span = build_profile::Span::new(&build_profile::FEEDS);
                let parser = parser.get_or_insert_with(|| {
                    let has_body = bytes_contain(chunk, b"<body") || bytes_contain(chunk, b":body");
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
        #[cfg(feature = "bench-serial")]
        drop(spine_span);
        if sink.interrupted {
            return Err(ReaderCacheError::Interrupted);
        }
        match read_result {
            Ok(()) => {
                if let Some(parser) = parser.as_mut() {
                    if let Err(err) = tokenizer.finish_xhtml_blocks(parser, &mut sink) {
                        if sink.interrupted {
                            return Err(ReaderCacheError::Interrupted);
                        }
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
                        "epub: bounded open stopped at spine {}: {:?}",
                        spine_index,
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
        if sink.write_failed {
            return Err(ReaderCacheError::CacheWrite);
        }
        if let (Some((first, last)), Some(images)) = (sink.unprobed, images) {
            let generated_toc = sink.generated_toc_for_spine;
            take_back_first_pass(library, run, spine_index, generated_toc);
            probe_spine_images(images, zip, first..=last, scratch, preparation, spine_index)?;
            return Ok(SpinePass::Again);
        }
        if !sink.run_full {
            sink.finish_spine(false);
        }
        if sink.write_failed {
            return Err(ReaderCacheError::CacheWrite);
        }
        Ok(SpinePass::Laid(if sink.run_full {
            SpineOutcome::RunFull
        } else {
            SpineOutcome::Laid
        }))
    })
}

/// Probes the images a spine's first pass marked, between its passes.
/// Out of line, so the spine's own frame, which the parse runs beneath,
/// does not carry this one's.
#[inline(never)]
fn probe_spine_images<Z, D, T, const ND: usize, const NF: usize, const NV: usize>(
    images: &BookImages<'_, D, T, ND, NF, NV>,
    zip: &mut Z,
    slots: core::ops::RangeInclusive<u16>,
    scratch: &mut SpineScratch<'_>,
    preparation: Preparation,
    spine_index: usize,
) -> Result<(), ReaderCacheError>
where
    Z: EpubZipOps,
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let started = Instant::now();
    let probed = images
        .probe_marked(
            zip,
            slots,
            &mut scratch.compressed[..],
            &mut scratch.xhtml[..],
            scratch.zip_inflate,
            || preparation.gives_way(),
        )
        .map_err(|stop| match stop {
            ProbeStop::Interrupted => ReaderCacheError::Interrupted,
            // A failed read of the book is worth retrying, as for a spine.
            ProbeStop::Archive => proto::epub::ZipError::Io.into(),
            ProbeStop::Index => ReaderCacheError::CacheWrite,
        })?;
    esp_println::println!(
        "epub: spine {} probed {} image(s) in {} ms; laying it out again",
        spine_index,
        probed,
        started.elapsed().as_millis()
    );
    Ok(())
}

/// Takes back what a spine's first pass added outside its section files,
/// before the second: nothing it added needs remembering beforehand. A
/// spine's sections are the tail of its run's table (a session lays each
/// spine out once, in order), and the TOC record a pass generates from a
/// heading is the list's last. The book's partial mark needs no undoing:
/// it only ever goes true, and the second pass, identical up to the first
/// pass's first unprobed image and adding nothing after it, sets it
/// wherever the first did.
fn take_back_first_pass(
    library: &mut ReaderStore,
    run: SpineRun,
    spine_index: usize,
    generated_toc: bool,
) {
    let spine = spine_index.min(u16::MAX as usize) as u16;
    match run {
        SpineRun::Prefix => library.book_sections.drop_trailing_spine(spine),
        SpineRun::Seed => library.seed.sections.drop_trailing_spine(spine),
    }
    if generated_toc && library.toc_count > 0 {
        library.toc_count -= 1;
        library.toc_text_len = library.toc[library.toc_count].title_offset as usize;
    }
    library.clear_lines();
}

/// How one layout pass over a spine ended.
#[derive(Clone, Copy)]
enum SpinePass {
    Laid(SpineOutcome),
    /// The pass met images no probe had read; they are probed now, and the
    /// pass taken back: lay the spine out again.
    Again,
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

/// Without a text reference or landmark, a first spine item that the guide
/// names as the cover page, or whose name says cover or title page, is front
/// matter; the text starts after it.
fn inferred_text_start_spine(package: &proto::epub::EpubPackage<'_>) -> Option<u16> {
    if package.spine.len() <= 1 {
        return None;
    }
    let first = package.spine.first()?;
    let first_href = first.href.of(package.opf_text);
    let lower_href = LowerAscii::<MAX_ENTRY_NAME_BYTES>::new(first_href);
    (package
        .cover_page_href
        .is_some_and(|cover| href_matches_spine(cover, first_href))
        || lower_href.contains("titlepage")
        || lower_href.contains("title-page")
        || lower_href.contains("cover"))
    .then_some(1)
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
