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
use app_core::{
    display_orientation_from_u8, refresh_policy_from_u8, AppView, DisplayOrientation, ReaderSource,
    RefreshPlanner, RenderKind, RenderRequest, SyncSession, SyncStatus,
};
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
    // The previous-frame buffer sits in dram2 so the radio's statics fit
    // in main DRAM; same exclusive &'static mut as the old local cell.
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");
    static TX_BAND: static_cell::StaticCell<[u8; BAND_BYTES]> = static_cell::StaticCell::new();
    let tx_band = TX_BAND.init([0; BAND_BYTES]);
    let mut epub_scratch = None;
    // Storage-command admission for the sync session lifecycle; the loan
    // transition and refusal rules live in app-core with the contracts.
    let mut sync_session = SyncSession::default();
    let mut refresh_planner = RefreshPlanner::new();
    let mut pending_progress: Option<AppStateRecord> = None;
    let mut last_progress_write: Option<Instant> = None;
    // STATE.BIN is consulted once per boot, after the first catalog with
    // entries lands; later catalog refreshes must not yank reading state.
    let mut state_restored = false;
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut prev_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();

    esp_println::println!("display: init start");
    display_flush::init_panel(&mut epd).await;
    esp_println::println!("display: init complete");

    // One-shot firmware self-update: if the card holds a pending image, flash it
    // into the inactive OTA slot and reboot into it before the reader starts.
    // Runs here because SD access lives behind this task's shared SPI bus, and
    // the radio is still idle so the flash writes are safe.
    match crate::sd_session::with_root(
        &mut epd,
        &mut sd_cs,
        crate::ota_update::apply_pending_update,
    ) {
        Ok(true) => {
            esp_println::println!("display: firmware update staged; resetting");
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            esp_hal::system::software_reset();
        }
        Ok(false) => {}
        Err(e) => esp_println::println!("display: update check skipped: {:?}", e),
    }

    // Flash-path self-test (feature `ota-selftest` only, off in release): copy
    // the running image into the inactive slot and boot into it, once. A card-
    // reader-free way to re-validate the esp-storage + otadata path on device.
    #[cfg(feature = "ota-selftest")]
    if crate::ota_update::run_selftest() {
        esp_println::println!("selftest: staged; resetting");
        embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
        esp_hal::system::software_reset();
    }

    loop {
        match select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()).await {
            Either::First(DisplayCommand::Render(request)) => {
                let content_context_changed = refresh_planner
                    .last_request()
                    .map(|last| (last.view, last.book_id))
                    != Some((request.view, request.book_id));
                // The catalog is streamed from the card, so make the slice this
                // view needs resident before the (pure) render reads it. Library
                // pulls the list window around the selection; other views need
                // the active book's entry, refreshed only when the book changes.
                // Skipped once the sync session is running.
                if !sync_session.active() {
                    if request.view == AppView::Library {
                        crate::library_sd::ensure_library_window(
                            &mut epd,
                            &mut sd_cs,
                            sd_library,
                            request.selection,
                            request.orientation.is_portrait(),
                        );
                    } else if ReaderSource::from_book_id(request.book_id).is_sd() {
                        if let Some(index) = ReaderStore::selected_book_index(request.book_id) {
                            if content_context_changed {
                                crate::library_sd::load_active_entry(
                                    &mut epd, &mut sd_cs, sd_library, index,
                                );
                            }
                            // Long TOCs are windowed like the catalog; slide
                            // the window over the rows this render will show.
                            if request.view == AppView::Chapters && sd_library.text_holds_toc() {
                                reader_cache::ensure_toc_window(
                                    &mut epd,
                                    &mut sd_cs,
                                    sd_library,
                                    index,
                                    request.selection as usize,
                                    request.orientation.is_portrait(),
                                );
                            }
                        }
                    }
                }
                let layout_start = Instant::now();
                if !render_custom_reader(&mut epd, &mut sd_cs, fb, request, sd_library) {
                    crate::views::render(fb, request, sd_library);
                }
                let layout_ms = layout_start.elapsed().as_millis();

                if !refresh_planner.screen_on() && refresh_planner.last_request().is_none() {
                    esp_println::println!("display: wake init start");
                    display_flush::init_panel(&mut epd).await;
                    esp_println::println!("display: wake init complete");
                    prev_prestaged = false;
                }

                let mode = refresh_planner.mode_for(request);
                if content_context_changed {
                    esp_println::println!(
                        "display: context changed, refresh policy {:?} -> {:?}",
                        request.refresh_policy,
                        mode
                    );
                }
                let flush_start = Instant::now();
                if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    mode,
                    prev_prestaged,
                )
                .await
                .is_ok()
                {
                    let flush_ms = flush_start.elapsed().as_millis();
                    refresh_planner.record_render(request, mode);
                    prev_fb.copy_from(fb);
                    let prestage_start = Instant::now();
                    prev_prestaged = display_flush::prestage_previous(&mut epd, fb, tx_band)
                        .await
                        .is_ok();
                    esp_println::println!(
                        "bench: render view={:?} mode={:?} page={} chapter={} layout_ms={} flush_ms={} prestage_ms={} t_ms={}",
                        request.view,
                        mode,
                        request.page,
                        request.chapter,
                        layout_ms,
                        flush_ms,
                        prestage_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    // Keep the current chapter tracking the page just shown, past
                    // the reducer's 128-chapter cap. Cheap in-RAM check; only the
                    // loaded SD reader has an uncapped page map, so this no-ops on
                    // other views and reads SD only when the chapter changes.
                    if request.view == AppView::Reading {
                        if let Some(current) = reader_cache::track_reading_chapter(
                            &mut epd,
                            &mut sd_cs,
                            request.page,
                            sd_library,
                        ) {
                            send_loaded_library_event(&LibraryEvent::ChapterCursor {
                                book_id: request.book_id,
                                current_chapter: current,
                            });
                        }
                    }
                    send_required_display_event(&DisplayEvent::Settled);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplaySettled);
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    prev_prestaged = false;
                    send_required_display_event(&DisplayEvent::Settled);
                }
            }
            Either::First(DisplayCommand::Sleep) => {
                let sleep_start = Instant::now();
                esp_println::println!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    sleep_start.as_millis(),
                );
                flush_pending_progress(
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    &mut pending_progress,
                    &mut last_progress_write,
                );
                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        &pending_progress,
                    )
                });
                if let Some(request) = request {
                    crate::views::render_sleep(fb, request, sd_library);
                } else {
                    crate::views::render_sleep_blank(fb);
                }
                let sleep_frame_settled = if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    RefreshMode::Full,
                    prev_prestaged,
                )
                .await
                .is_ok()
                {
                    prev_fb.copy_from(fb);
                    esp_println::println!(
                        "bench: sleep phase=refresh ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    true
                } else {
                    esp_println::println!("display: sleep framebuffer flush failed");
                    esp_println::println!(
                        "bench: sleep phase=refresh ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                    false
                };
                prev_prestaged = false;
                if display_flush::sleep_panel(&mut epd).await.is_ok() {
                    if sleep_frame_settled {
                        refresh_planner.record_sleep();
                    }
                    send_required_display_event(&DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplayAsleep);
                    esp_println::println!(
                        "bench: sleep phase=complete ok=true elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                } else {
                    esp_println::println!("display: sleep command failed");
                    send_required_display_event(&DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.try_send(PowerEvent::DisplayAsleep);
                    esp_println::println!(
                        "bench: sleep phase=complete ok=false elapsed_ms={} t_ms={}",
                        sleep_start.elapsed().as_millis(),
                        Instant::now().as_millis(),
                    );
                }
            }
            Either::Second(StorageCommand::ReceiveUpload) => {
                if sync_session.admits(&StorageCommand::ReceiveUpload) {
                    // Diverges: the display task becomes the upload writer
                    // for the rest of the sync session.
                    crate::sd_session::upload_session(&mut epd, &mut sd_cs).await;
                }
                esp_println::println!("storage: upload refused outside sync");
            }
            Either::Second(command) => {
                // A layout change re-paginates the book, which blocks this
                // task for the whole rebuild. Paint the title/author plate
                // first so the wait reads as loading, not frozen: the store
                // still reports the old settings here, so the reader view
                // lands on the loading branch. A same-layout open already
                // shows the plate through the normal render path (the book
                // isn't loaded yet), so it is skipped here.
                if refresh_planner.screen_on() {
                    if let Some(loading_request) = open_loading_plate_request(
                        &command,
                        sd_library,
                        refresh_planner.last_request(),
                    ) {
                        crate::views::render(fb, loading_request, sd_library);
                        let mode = refresh_planner.mode_for(loading_request);
                        if display_flush::flush(
                            &mut epd,
                            fb,
                            prev_fb,
                            tx_band,
                            refresh_planner.screen_on(),
                            mode,
                            prev_prestaged,
                        )
                        .await
                        .is_ok()
                        {
                            refresh_planner.record_render(loading_request, mode);
                            prev_fb.copy_from(fb);
                            prev_prestaged = false;
                        }
                    }
                }
                handle_storage_command(
                    command,
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    &mut epub_scratch,
                    &mut sync_session,
                    &mut pending_progress,
                    &mut last_progress_write,
                    &mut state_restored,
                    refresh_planner.last_request(),
                );
            }
        }
    }
}

pub(crate) fn send_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: library event queue full");
    }
}

fn render_custom_reader(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
) -> bool {
    if request.view != AppView::Reading
        || !ReaderSource::from_book_id(request.book_id).is_sd()
        || request.font_family != display::font::FontFamily::Custom
        || display::font::builtin_custom_available()
        || !sd_library.custom_font_available()
    {
        return false;
    }
    crate::sd_session::with_root(epd, sd_cs, |root| {
        crate::views::render_custom_reader_from_root(fb, request, sd_library, root)
    })
    .unwrap_or(false)
}

/// The reader-view render to paint as a loading plate before an open/extend
/// that cannot be answered from the already loaded RAM section. The app sends
/// a normal Reading render around the same time, but the storage receiver can
/// win that race; painting here keeps a first cache build from looking frozen
/// on the previous screen.
fn open_loading_plate_request(
    command: &StorageCommand,
    sd_library: &ReaderStore,
    last_request: Option<RenderRequest>,
) -> Option<RenderRequest> {
    let (book_id, index, target_pages, type_settings) = match *command {
        StorageCommand::OpenBook {
            book_id,
            index,
            target_pages,
            type_settings,
            ..
        } => (book_id, index, target_pages, type_settings),
        StorageCommand::ExtendSection {
            book_id,
            index,
            target_pages,
            type_settings,
            ..
        } => (book_id, index, target_pages, type_settings),
        _ => return None,
    };
    // Only SD books re-paginate and route to the reader loading plate; the
    // built-in book renders from embedded content and never rebuilds.
    if !ReaderSource::from_book_id(book_id).is_sd() {
        return None;
    }
    if sd_library.type_settings() == type_settings
        && sd_library.covers_global_page(index as usize, target_pages as u32)
    {
        return None;
    }
    let mut request = last_request?;
    request.view = AppView::Reading;
    request.book_id = book_id;
    request.page = target_pages as u32;
    request.font_size = type_settings.size;
    request.line_spacing = type_settings.spacing;
    request.font_weight = type_settings.weight;
    request.font_family = type_settings.family;
    // The command carries only a portrait bool, so preserve the request's
    // existing variant when it already agrees and reconcile only a mismatch
    // (a stale landscape template on a portrait open, or vice versa).
    request.orientation = match (type_settings.portrait, request.orientation.is_portrait()) {
        (true, false) => DisplayOrientation::PortraitButtonsRight,
        (false, true) => DisplayOrientation::LandscapeButtonsBottom,
        _ => request.orientation,
    };
    Some(request)
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
    sync_session: &mut SyncSession,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
    state_restored: &mut bool,
    last_request: Option<RenderRequest>,
) {
    let is_open_book = matches!(command, StorageCommand::OpenBook { .. });
    // The session decides what may run: progress writes stay alive during a
    // sync session (they are cheap and harmless); everything
    // that touches the EPUB scratch is gone until the session's reset.
    if !sync_session.admits(&command) {
        esp_println::println!("storage: refused during sync session");
        return;
    }
    match command {
        StorageCommand::LoanSyncMemory => {
            // The session only ends in a reset, so any coalesced position
            // must reach the card before the scratch is dismantled.
            flush_pending_progress(
                epd,
                sd_cs,
                sd_library,
                pending_progress,
                last_progress_write,
            );
            ensure_epub_scratch(epub_scratch);
            let Some(scratch) = epub_scratch.take() else {
                return;
            };
            sync_session.loan_granted();
            let mut loan = reader_cache::dismantle_scratch(scratch);
            loan.wifi = reader_cache::load_wifi_credentials(epd, sd_cs).map(|record| {
                app_core::WifiCredentials {
                    ssid: record.ssid,
                    ssid_len: record.ssid_len,
                    password: record.password,
                    password_len: record.password_len,
                }
            });
            loan.catalog_len = crate::library_sd::write_catalog_listing(epd, sd_cs, loan.http_b);
            if crate::SYNC_LOANS.try_send(loan).is_err() {
                // Unreachable in practice: the wifi task requests exactly
                // one loan per boot. The memory is gone either way.
                esp_println::println!("storage: sync loan channel full");
            }
        }
        StorageCommand::LoadCatalogCache => {
            // Boot-time probe: name the saved network so the Wireless
            // screen can offer connect/forget honestly. The command runs
            // once per boot, before any session can start.
            if let Some(record) = reader_cache::load_wifi_credentials(epd, sd_cs) {
                let ssid = app_core::WifiSsid {
                    bytes: record.ssid,
                    len: record.ssid_len,
                };
                esp_println::println!("wifi: saved network '{}'", ssid.as_str());
                let _ = crate::SYNC_EVENTS.try_send(crate::SyncEvent::NetworkSaved(ssid));
            } else {
                esp_println::println!("wifi: no saved network");
            }
            reader_cache::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            if crate::library_sd::load_catalog_cache(epd, sd_cs, sd_library) {
                // Restored goes out first so the very next Home repaint
                // already shows the saved book; the Scanned default then
                // sees an SD book active and leaves it alone.
                restore_saved_state(epd, sd_cs, sd_library, state_restored);
                let count = sd_library.catalog_count_u16();
                send_library_event(&LibraryEvent::Scanned { count });
            } else {
                let _ = STORAGE_COMMANDS.try_send(StorageCommand::RefreshCatalog);
            }
        }
        StorageCommand::RefreshCatalog => {
            reader_cache::load_custom_font_manifest(epd, sd_cs, sd_library);
            send_library_event(&LibraryEvent::CustomFont {
                available: sd_library.custom_font_available(),
            });
            crate::library_sd::scan_books(epd, sd_cs, sd_library);
            restore_saved_state(epd, sd_cs, sd_library, state_restored);
            send_library_event(&LibraryEvent::Scanned {
                count: sd_library.catalog_count_u16(),
            });
        }
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
        } => {
            let storage_start = Instant::now();
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
            // Read this book's catalog record into the active-entry slot so the
            // reader pipeline (load_position, build_or_load) resolves it from
            // the card rather than the list window. A failure leaves the entry
            // unset and the open falls through to the usual bad-index error.
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            // Adopt the command's type settings before the RAM fast path:
            // a settings change drops the loaded page coverage, so the
            // request falls through to the cache load/rebuild below.
            sd_library.set_type_settings(type_settings);
            // A fresh selection (chapter 0, page 0) resumes from the
            // book's own saved position; explicit page requests pass
            // through untouched. Extends never resume.
            let mut chapter = chapter;
            let mut target_pages = target_pages;
            let mut resumed = false;
            if is_open_book && chapter == 0 && target_pages == 0 {
                if let Some((saved_chapter, saved_screen)) =
                    reader_cache::load_position(epd, sd_cs, sd_library, index as usize)
                {
                    if saved_chapter > 0 || saved_screen > 0 {
                        chapter = saved_chapter.min(u8::MAX as u16) as u8;
                        target_pages = saved_screen.min(u16::MAX as u32) as u16;
                        resumed = true;
                        esp_println::println!(
                            "storage: resume book {} at chapter {} screen {}",
                            book_id,
                            chapter,
                            target_pages
                        );
                    }
                }
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
                esp_println::println!(
                    "bench: storage_open request={} book_id={} index={} ram_hit=true elapsed_ms={} pages={} chapters={}",
                    request_id,
                    book_id,
                    index,
                    storage_start.elapsed().as_millis(),
                    sd_library.advertised_page_count(),
                    sd_library.chapter_count_for_ui(),
                );
                send_loaded_library_event(&LibraryEvent::Loaded {
                    book_id,
                    pages: sd_library.advertised_page_count(),
                    chapters: sd_library.chapter_count_for_ui(),
                    current_chapter: sd_library.current_chapter(),
                    chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                });
                if resumed {
                    send_resumed_position(book_id, chapter, target_pages, last_request);
                }
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
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
            });
            if resumed {
                send_resumed_position(book_id, chapter, target_pages, last_request);
            }
            esp_println::println!(
                "storage: open complete status={:?} pages={} chapters={}",
                sd_library.reader_status(),
                sd_library.advertised_page_count(),
                sd_library.chapter_count_for_ui()
            );
            esp_println::println!(
                "bench: storage_open request={} book_id={} index={} ram_hit=false elapsed_ms={} status={:?} pages={} chapters={}",
                request_id,
                book_id,
                index,
                storage_start.elapsed().as_millis(),
                sd_library.reader_status(),
                sd_library.advertised_page_count(),
                sd_library.chapter_count_for_ui(),
            );
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            // The overview opens with the cursor on the current chapter, so
            // center the first TOC window there.
            let ok = reader_cache::load_chapters_into_store(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                sd_library.current_chapter() as usize,
            );
            esp_println::println!(
                "storage: chapters loaded book_id={} ok={} count={}",
                book_id,
                ok,
                sd_library.overview_chapter_count()
            );
            // Re-render the overview with the full list resident, syncing the
            // selection range to the full chapter count.
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
            });
        }
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            type_settings,
        } => {
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            sd_library.set_type_settings(type_settings);
            // The TOC is still in the buffer; resolve the chapter's start page
            // before loading the section overwrites it. Re-ensure the window
            // covers the selection in case it slid since the overview render.
            reader_cache::ensure_toc_window(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter as usize,
                true, // Over-estimate visible rows to ensure chapter is covered
            );
            let target_page = sd_library.overview_page_at(chapter as usize);
            let scratch = ensure_epub_scratch(epub_scratch);
            reader_cache::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_page as usize,
                scratch,
            );
            send_loaded_library_event(&LibraryEvent::Loaded {
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
            });
            send_resumed_position(book_id, chapter, target_page, last_request);
        }
        StorageCommand::ReceiveUpload => {
            // Handled in the task loop before dispatch; reaching here means
            // the loop refused it already.
        }
        StorageCommand::StoreWifiCredentials(credentials) => {
            let stored = reader_cache::store_wifi_credentials(
                epd,
                sd_cs,
                hal_ext::nvm::WifiCredentialsRecord {
                    ssid: credentials.ssid,
                    ssid_len: credentials.ssid_len,
                    password: credentials.password,
                    password_len: credentials.password_len,
                },
            );
            esp_println::println!("storage: wifi credentials stored={}", stored);
        }
        StorageCommand::ForgetWifiCredentials => {
            let forgotten = reader_cache::forget_wifi_credentials(epd, sd_cs);
            esp_println::println!("storage: wifi credentials forgotten={}", forgotten);
        }
        StorageCommand::StoreProgress(record) => {
            let (source_hash, source_size) = source_identity(sd_library, record.book_id);
            // The reducer derives chapter from the 128-capped sd_chapter_for_page,
            // so a deep position would save a stuck chapter that the sleep/boot
            // colophon then shows wrong until the book reopens. The firmware
            // tracks the true chapter over the whole book; adopt it for the
            // loaded SD book so saved/restored state names the chapter right.
            let chapter = if ReaderSource::from_book_id(record.book_id).is_sd()
                && sd_library.loaded_index == ReaderStore::selected_book_index(record.book_id)
            {
                sd_library.current_chapter()
            } else {
                record.chapter
            };
            let record = AppStateRecord {
                book_id: record.book_id,
                chapter,
                screen: record.screen,
                shell_orientation: record.shell_orientation,
                reading_orientation: record.reading_orientation,
                refresh_policy: record.refresh_policy,
                font_size: record.font_size,
                line_spacing: record.line_spacing,
                font_weight: record.font_weight,
                font_family: record.font_family,
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
                flush_pending_progress(
                    epd,
                    sd_cs,
                    sd_library,
                    pending_progress,
                    last_progress_write,
                );
            }
            if context_changed || due {
                let progress_start = Instant::now();
                reader_cache::store_app_state(epd, sd_cs, sd_library, record);
                *pending_progress = None;
                *last_progress_write = Some(Instant::now());
                esp_println::println!(
                    "bench: storage_progress action=write book_id={} page={} elapsed_ms={} t_ms={}",
                    record.book_id,
                    record.screen,
                    progress_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            } else {
                *pending_progress = Some(record);
                esp_println::println!(
                    "bench: storage_progress action=coalesce book_id={} page={} t_ms={}",
                    record.book_id,
                    record.screen,
                    Instant::now().as_millis(),
                );
            }
        }
    }
}

fn send_required_library_event(event: &LibraryEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if LIBRARY_EVENTS.try_send(*event).is_ok() {
            return;
        }
        let _ = LIBRARY_EVENTS.try_receive();
    }
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: required library event queue full");
    }
}

fn send_loaded_library_event(event: &LibraryEvent) {
    if DISPLAY_EVENTS
        .try_send(DisplayEvent::Library(*event))
        .is_ok()
    {
        return;
    }
    send_required_library_event(event);
}

fn send_required_display_event(event: &DisplayEvent) {
    const RETRIES: usize = 8;
    for _ in 0..RETRIES {
        if DISPLAY_EVENTS.try_send(*event).is_ok() {
            return;
        }
        if let Ok(DisplayEvent::Library(library_event)) = DISPLAY_EVENTS.try_receive() {
            send_required_library_event(&library_event);
        }
    }
    if DISPLAY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: required display event queue full");
    }
}

/// Announces a per-book resume to the app as a Restored event, carrying
/// the current display settings unchanged.
fn send_resumed_position(
    book_id: u32,
    chapter: u8,
    target_pages: u16,
    last_request: Option<RenderRequest>,
) {
    let Some(request) = last_request else {
        return;
    };
    send_required_library_event(&LibraryEvent::Restored {
        book_id,
        chapter,
        page: target_pages as u32,
        // The book is loaded here, so carry its known total page count.
        page_count: request.page_count,
        reading_orientation: request.orientation as u8,
        refresh_policy: request.refresh_policy as u8,
        font_size: request.font_size as u8,
        line_spacing: request.line_spacing as u8,
        font_weight: request.font_weight as u8,
        font_family: request.font_family as u8,
    });
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

/// One boot-time attempt to map `/XTEINK/STATE.BIN` back onto the scanned
/// catalog by stable source identity (path hash + byte size) and hand the
/// saved position to the app as a `Restored` event. The volatile book id
/// stored in the record is never trusted directly.
fn restore_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    state_restored: &mut bool,
) {
    if *state_restored || library.catalog_is_empty() {
        return;
    }
    *state_restored = true;
    let Some(record) = reader_cache::load_app_state(epd, sd_cs) else {
        esp_println::println!("restore: no usable STATE.BIN");
        return;
    };
    let hint = ReaderSource::from_book_id(record.book_id).sd_index();
    let Some(index) = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        hint,
    ) else {
        esp_println::println!(
            "restore: no catalog match hash={:08x} size={}",
            record.source_hash,
            record.source_size
        );
        return;
    };
    esp_println::println!(
        "restore: index={} chapter={} screen={}",
        index,
        record.chapter,
        record.screen
    );
    // Stage the restored book's catalog entry so the colophon/page-count reads
    // below resolve it, and so the first Home paint names it before any open.
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    // Resolve the chapter title now so wake-to-Home (rendered before the book
    // is opened) names the chapter; without this the colophon shows a numeral
    // until the book is first opened this session.
    reader_cache::load_chapter_title(epd, sd_cs, usize::from(index), record.chapter, library);
    // The book's total page count, so the Home progress bar has a denominator
    // on wake before the book is opened (read from the cache index header).
    let page_count = reader_cache::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    send_required_library_event(&LibraryEvent::Restored {
        book_id: ReaderSource::sd(index).book_id(),
        chapter: record.chapter.min(u8::MAX as u16) as u8,
        page: record.screen,
        page_count,
        reading_orientation: record.reading_orientation,
        refresh_policy: record.refresh_policy,
        font_size: record.font_size,
        line_spacing: record.line_spacing,
        font_weight: record.font_weight,
        font_family: record.font_family,
    });
}

fn sleep_request_from_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    pending_progress: &Option<AppStateRecord>,
) -> Option<RenderRequest> {
    let record = match *pending_progress {
        Some(record) => record,
        None => reader_cache::load_app_state(epd, sd_cs)?,
    };
    let hint = ReaderSource::from_book_id(record.book_id).sd_index();
    let index = crate::library_sd::find_index_by_identity(
        epd,
        sd_cs,
        record.source_hash,
        record.source_size,
        hint,
    )?;
    crate::library_sd::load_active_entry(epd, sd_cs, library, usize::from(index));
    reader_cache::load_chapter_title(epd, sd_cs, usize::from(index), record.chapter, library);
    let page_count = reader_cache::restore_book_page_count(epd, sd_cs, usize::from(index), library);
    let orientation = display_orientation_from_u8(record.reading_orientation)
        .unwrap_or(DisplayOrientation::LandscapeButtonsBottom);
    Some(RenderRequest {
        kind: RenderKind::Page,
        view: AppView::Home,
        page: record.screen,
        page_count,
        chapter: record.chapter.min(u8::MAX as u16) as u8,
        selection: 0,
        book_id: ReaderSource::sd(index).book_id(),
        orientation,
        reading_sheet: false,
        refresh_policy: refresh_policy_from_u8(record.refresh_policy)
            .unwrap_or(app_core::RefreshPolicy::FullOnWake),
        font_size: display::font::FontSize::from_u8(record.font_size)
            .unwrap_or(display::font::FontSize::Medium),
        line_spacing: display::font::LineSpacing::from_u8(record.line_spacing)
            .unwrap_or(display::font::LineSpacing::Normal),
        font_weight: display::font::FontWeight::from_u8(record.font_weight)
            .unwrap_or(display::font::FontWeight::Normal),
        font_family: display::font::FontFamily::from_u8(record.font_family)
            .unwrap_or(display::font::FontFamily::Literata),
        last_button: None,
        aux_raw: 0,
        nav_raw: 0,
        page_raw: 0,
        battery_mv: 0,
        battery_percent: 100,
        library_count: library.catalog_count_u16(),
        sync_status: SyncStatus::NotConfigured,
        wifi_ssid: [0; 32],
        wifi_ssid_len: 0,
        dirty: display::Rect::full_for_orientation(orientation.is_portrait()),
    })
}

fn flush_pending_progress(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    pending_progress: &mut Option<AppStateRecord>,
    last_progress_write: &mut Option<Instant>,
) {
    if let Some(record) = pending_progress.take() {
        let start = Instant::now();
        reader_cache::store_app_state(epd, sd_cs, sd_library, record);
        *last_progress_write = Some(Instant::now());
        esp_println::println!(
            "bench: storage_progress action=flush book_id={} page={} elapsed_ms={} t_ms={}",
            record.book_id,
            record.screen,
            start.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
    }
}
