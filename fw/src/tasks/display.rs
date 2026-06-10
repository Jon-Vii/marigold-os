use crate::display_flush::{self, Epd};
use crate::reader_cache::{
    self, ReaderCacheScratch, READER_COMPRESSED_SCRATCH, READER_CONTAINER_SCRATCH,
    READER_HEADER_SCRATCH, READER_OPF_SCRATCH, READER_TAIL_SCRATCH, READER_XHTML_SCRATCH,
};
use crate::reader_store::{
    BookLoadStatus, ReaderStore, EMPTY_BOOK_SECTION_RECORD, MAX_BOOK_SECTIONS,
};
use crate::{
    DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, StorageCommand, DISPLAY_COMMANDS,
    DISPLAY_EVENTS, LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::RefreshPlanner;
use core::sync::atomic::Ordering;
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::BAND_BYTES;
use embassy_futures::select::{select, Either};
use embassy_time::Instant;
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;
use static_cell::ConstStaticCell;

/// Same-book page-turn progress is coalesced: at most one STATE.BIN write
/// per this interval, with a guaranteed flush before display sleep. A
/// battery pull can lose at most this many seconds of reading position.
const PROGRESS_WRITE_MIN_SECS: u64 = 15;

static EPUB_TAIL: ConstStaticCell<[u8; READER_TAIL_SCRATCH]> =
    ConstStaticCell::new([0; READER_TAIL_SCRATCH]);
static EPUB_HEADER: ConstStaticCell<[u8; READER_HEADER_SCRATCH]> =
    ConstStaticCell::new([0; READER_HEADER_SCRATCH]);
static EPUB_NAME: ConstStaticCell<[u8; proto::epub::MAX_ENTRY_NAME_BYTES]> =
    ConstStaticCell::new([0; proto::epub::MAX_ENTRY_NAME_BYTES]);
static EPUB_COMPRESSED: ConstStaticCell<[u8; READER_COMPRESSED_SCRATCH]> =
    ConstStaticCell::new([0; READER_COMPRESSED_SCRATCH]);
static EPUB_CONTAINER: ConstStaticCell<[u8; READER_CONTAINER_SCRATCH]> =
    ConstStaticCell::new([0; READER_CONTAINER_SCRATCH]);
static EPUB_OPF: ConstStaticCell<[u8; READER_OPF_SCRATCH]> =
    ConstStaticCell::new([0; READER_OPF_SCRATCH]);
static EPUB_XHTML: ConstStaticCell<[u8; READER_XHTML_SCRATCH]> =
    ConstStaticCell::new([0; READER_XHTML_SCRATCH]);
static EPUB_BOOK_SECTIONS: ConstStaticCell<[proto::cache::BookV2SectionRecord; MAX_BOOK_SECTIONS]> =
    ConstStaticCell::new([EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS]);
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

#[embassy_executor::task]
pub async fn run(mut epd: Epd, mut sd_cs: Output<'static>) {
    esp_println::println!("display: started");

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    static PREV_FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let prev_fb = PREV_FB.init(Framebuffer::new());
    static TX_BAND: static_cell::StaticCell<[u8; BAND_BYTES]> = static_cell::StaticCell::new();
    let tx_band = TX_BAND.init([0; BAND_BYTES]);
    let mut epub_scratch = None;
    let mut refresh_planner = RefreshPlanner::new();
    let mut pending_progress: Option<AppStateRecord> = None;
    let mut last_progress_write: Option<Instant> = None;
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut red_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();

    esp_println::println!("display: init start");
    display_flush::init_panel(&mut epd).await;
    esp_println::println!("display: init complete");
    loop {
        match select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()).await {
            Either::First(DisplayCommand::Render(request)) => {
                let content_context_changed = refresh_planner
                    .last_request()
                    .map(|last| (last.view, last.book_id))
                    != Some((request.view, request.book_id));
                crate::views::render(fb, request, sd_library);

                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("display: wake init start");
                    display_flush::init_panel(&mut epd).await;
                    esp_println::println!("display: wake init complete");
                    red_prestaged = false;
                }

                let mode = refresh_planner.mode_for(request);
                if content_context_changed {
                    esp_println::println!(
                        "display: context changed, refresh policy {:?} -> {:?}",
                        request.refresh_policy,
                        mode
                    );
                }
                if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    mode,
                    red_prestaged,
                )
                .await
                .is_ok()
                {
                    refresh_planner.record_render(request, mode);
                    prev_fb.copy_from(fb);
                    red_prestaged = display_flush::prestage_red(&mut epd, fb, tx_band)
                        .await
                        .is_ok();
                    send_required_display_event(DisplayEvent::Settled);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplaySettled);
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    red_prestaged = false;
                    send_required_display_event(DisplayEvent::Settled);
                }
            }
            Either::First(DisplayCommand::Sleep) => {
                flush_pending_progress(
                    &mut epd,
                    &mut sd_cs,
                    &mut pending_progress,
                    &mut last_progress_write,
                );
                if let Some(request) = refresh_planner.last_request() {
                    crate::views::render_sleep(fb, request, sd_library);
                    let _ = display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        tx_band,
                        refresh_planner.screen_on(),
                        RefreshMode::Full,
                        red_prestaged,
                    )
                    .await;
                    prev_fb.copy_from(fb);
                }
                red_prestaged = false;
                if display_flush::sleep_panel(&mut epd).await.is_ok() {
                    refresh_planner.record_sleep();
                    send_required_display_event(DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplayAsleep);
                } else {
                    esp_println::println!("display: sleep command failed");
                    send_required_display_event(DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplayAsleep);
                }
            }
            Either::Second(command) => {
                handle_storage_command(
                    command,
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    &mut epub_scratch,
                    &mut pending_progress,
                    &mut last_progress_write,
                );
            }
        }
    }
}

pub(crate) fn send_library_event(event: LibraryEvent) {
    if LIBRARY_EVENTS.try_send(event).is_err() {
        esp_println::println!("display: library event queue full");
    }
}

/// Kept out of line so the task loop's poll frame stays small; the storage
/// arms below carry multi-KB scratch and run near the stack floor.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn handle_storage_command(
    command: StorageCommand,
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
) {
    match command {
        StorageCommand::LoadCatalogCache => {
            if crate::library_sd::load_catalog_cache(epd, sd_cs, sd_library) {
                let count = sd_library.catalog_count_u8();
                send_library_event(LibraryEvent::Scanned { count });
            } else {
                let _ = STORAGE_COMMANDS.try_send(StorageCommand::RefreshCatalog);
            }
        }
        StorageCommand::RefreshCatalog => {
            crate::library_sd::scan_books(epd, sd_cs, sd_library);
            send_library_event(LibraryEvent::Scanned {
                count: sd_library.catalog_count_u8(),
            });
        }
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                esp_println::println!(
                    "storage: stale open skipped request={} latest={} book_id={} index={}",
                    request_id,
                    LATEST_READER_REQUEST_ID.load(Ordering::Relaxed),
                    book_id,
                    index
                );
                return;
            }
            // The requested page is usually inside the section window that
            // is already loaded; answering from RAM keeps ordinary page
            // turns free of card init, FAT, and cache-file traffic.
            if sd_library.covers_global_page(index as usize, target_pages as u32) {
                esp_println::println!(
                    "storage: open hit in RAM request={} book_id={} page={}",
                    request_id,
                    book_id,
                    target_pages
                );
                send_loaded_library_event(LibraryEvent::Loaded {
                    book_id,
                    pages: sd_library.advertised_page_count(),
                    chapters: sd_library.chapter_count_for_ui(),
                    chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                });
                return;
            }
            esp_println::println!(
                "storage: open command request={} book_id={} index={} chapter={} target={}",
                request_id,
                book_id,
                index,
                chapter,
                target_pages
            );
            sd_library.set_reader_status(BookLoadStatus::Loading);
            let scratch = ensure_epub_scratch(epub_scratch);
            reader_cache::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_pages as usize,
                scratch,
            );
            send_loaded_library_event(LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
            });
            esp_println::println!(
                "storage: open complete status={:?} pages={} chapters={}",
                sd_library.reader_status(),
                sd_library.advertised_page_count(),
                sd_library.chapter_count_for_ui()
            );
        }
        StorageCommand::StoreProgress(record) => {
            let (source_hash, source_size) = source_identity(sd_library, record.book_id);
            let record = AppStateRecord {
                book_id: record.book_id,
                chapter: record.chapter,
                screen: record.screen,
                shell_orientation: record.shell_orientation,
                reading_orientation: record.reading_orientation,
                refresh_policy: record.refresh_policy,
                source_hash,
                source_size,
            };
            // Coalesce same-context page turns; anything beyond the screen
            // number changing (book, chapter, orientation, policy) is rare
            // and worth landing immediately. A pending record for the same
            // book is superseded by the new one; only a different book's
            // pending position must be preserved first.
            let context_changed = pending_progress
                .map(|pending| {
                    AppStateRecord {
                        screen: record.screen,
                        ..pending
                    } != record
                })
                .unwrap_or(false);
            let due = last_progress_write
                .map(|written| written.elapsed().as_secs() >= PROGRESS_WRITE_MIN_SECS)
                .unwrap_or(true);
            if pending_progress
                .map(|pending| pending.book_id != record.book_id)
                .unwrap_or(false)
            {
                flush_pending_progress(epd, sd_cs, pending_progress, last_progress_write);
            }
            if context_changed || due {
                reader_cache::store_app_state(epd, sd_cs, record);
                *pending_progress = None;
                *last_progress_write = Some(Instant::now());
            } else {
                *pending_progress = Some(record);
            }
        }
    }
}

fn send_required_library_event(event: LibraryEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if LIBRARY_EVENTS.try_send(event).is_ok() {
            return;
        }
        let _ = LIBRARY_EVENTS.try_receive();
    }
    if LIBRARY_EVENTS.try_send(event).is_err() {
        esp_println::println!("display: required library event queue full");
    }
}

fn send_loaded_library_event(event: LibraryEvent) {
    if DISPLAY_EVENTS
        .try_send(DisplayEvent::Library(event))
        .is_ok()
    {
        return;
    }
    send_required_library_event(event);
}

fn send_required_display_event(event: DisplayEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if DISPLAY_EVENTS.try_send(event).is_ok() {
            return;
        }
        if let Ok(DisplayEvent::Library(library_event)) = DISPLAY_EVENTS.try_receive() {
            send_required_library_event(library_event);
        }
    }
    if DISPLAY_EVENTS.try_send(event).is_err() {
        esp_println::println!("display: required display event queue full");
    }
}

/// Kept out of line: first-call initialization moves a multi-KB scratch
/// value into the static; that spike must not sit at the base of the EPUB
/// open call chain's frame.
#[inline(never)]
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    if epub_scratch.is_none() {
        esp_println::println!("storage: init epub scratch");
        *epub_scratch = Some(EPUB_SCRATCH.init_with(|| {
            ReaderCacheScratch::new(
                EPUB_TAIL.take(),
                EPUB_HEADER.take(),
                EPUB_NAME.take(),
                EPUB_COMPRESSED.take(),
                EPUB_CONTAINER.take(),
                EPUB_OPF.take(),
                EPUB_XHTML.take(),
                EPUB_BOOK_SECTIONS.take(),
            )
        }));
    }
    epub_scratch.as_deref_mut().unwrap()
}

fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    library.source_identity(book_id)
}

fn flush_pending_progress(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
) {
    if let Some(record) = pending_progress.take() {
        reader_cache::store_app_state(epd, sd_cs, record);
        *last_progress_write = Some(Instant::now());
    }
}
