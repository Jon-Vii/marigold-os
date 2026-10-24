use crate::display_flush::{self, Epd};
use crate::image_decode::{IdleImageStep, IdleImageWork};
use crate::reader_cache::{
    self, ImagePass, Preparation, ReaderCacheScratch, READER_COMPRESSED_SCRATCH,
    READER_CONTAINER_SCRATCH, READER_HEADER_SCRATCH, READER_OPF_SCRATCH, READER_TAIL_SCRATCH,
    READER_XHTML_SCRATCH,
};
use crate::reader_store::{BookLoadStatus, ReaderStore};
use crate::{
    DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, StorageCommand, BUTTON_PRESSES,
    DISPLAY_COMMANDS, DISPLAY_EVENTS, LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS,
    STORAGE_COMMANDS,
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
use embassy_time::{Duration, Instant};
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;
use static_cell::ConstStaticCell;

/// Same-book page-turn progress is coalesced: at most one durable state write
/// per this interval. A coalesced position is written when the interval
/// ends even if no further page is turned, and before display sleep, so a
/// battery pull loses at most this many seconds of reading position.
const PROGRESS_WRITE_MIN_SECS: u64 = 15;
/// Spacing between retries of a position write that failed (card pulled or
/// a transient fault), so a missing card cannot spin the task.
const PROGRESS_RETRY_SECS: u64 = 30;
/// How long the reader must leave the device alone before the rest of an
/// open book is prepared. Steps give way to a press anyway; the wait keeps
/// a quick run of page turns from paying for one that was just started.
const IDLE_PREPARATION_DELAY: Duration = Duration::from_millis(2000);

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
static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch<'static>> =
    static_cell::StaticCell::new();

#[embassy_executor::task]
pub async fn run(
    mut epd: Epd,
    mut sd_cs: Output<'static>,
    scratch: &'static mut ReaderCacheScratch<'static>,
) {
    esp_println::println!("display: started");

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    // The previous-frame buffer sits in dram2 so the radio's statics fit
    // in main DRAM; same exclusive &'static mut as the old local cell.
    let prev_fb = crate::sync_mem::take_prev_fb().expect("prev_fb claimed once");
    static TX_BAND: static_cell::StaticCell<[u8; BAND_BYTES]> = static_cell::StaticCell::new();
    let tx_band = TX_BAND.init([0; BAND_BYTES]);
    // Present from boot until a sync session borrows it for the radio; that
    // session ends in a reset, so it never needs rebuilding.
    let mut epub_scratch = Some(scratch);
    // Storage-command admission for the sync session lifecycle; the loan
    // transition and refusal rules live in app-core with the contracts.
    let mut sync_session = SyncSession::default();
    let mut refresh_planner = RefreshPlanner::new();
    // Waking from deep sleep is a reboot; the mark says the panel still
    // shows the sleep screen, so the first paint can take the quick clean.
    let woke_on_sleep_screen = crate::wake_mark::take();
    if woke_on_sleep_screen {
        refresh_planner.record_sleep();
    }
    esp_println::println!("bench: wake sleep_screen={}", woke_on_sleep_screen);
    let mut progress = ProgressJournal::new();
    // Durable state is consulted once per boot, after the first catalog with
    // entries lands; later catalog refreshes must not yank reading state.
    let mut state_restored = false;
    // True while RED RAM is known to hold exactly prev_fb's content, letting
    // a fast refresh skip its previous-frame stream. Reset on any failure,
    // sleep, or panel re-init; false just means the next flush writes RED.
    let mut prev_prestaged = false;
    static SD_LIBRARY: ConstStaticCell<ReaderStore> = ConstStaticCell::new(ReaderStore::new());
    let sd_library = SD_LIBRARY.take();
    // ASCII glyph metrics for the custom font pack; shared by the build's
    // line measurement and the reading-page draw so both stay off the card.
    static FONT_METRICS: ConstStaticCell<crate::custom_font::MetricCache> =
        ConstStaticCell::new(crate::custom_font::MetricCache::new());
    let font_metrics = FONT_METRICS.take();

    esp_println::println!("display: init start");
    loop {
        match display_flush::init_panel(&mut epd).await {
            Ok(()) => break,
            Err(error) => {
                esp_println::println!("display: init failed: {:?}; retrying", error);
                embassy_time::Timer::after_secs(1).await;
            }
        }
    }
    esp_println::println!("display: init complete");
    // The controller is initialised; it needs it again only after a sleep
    // powers it down. Boot used to initialise it twice before the first
    // paint.
    let mut panel_ready = true;

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

    #[cfg(feature = "bench-serial")]
    {
        let result = crate::sd_session::with_root(&mut epd, &mut sd_cs, |root| {
            crate::custom_font::bench::run(root, fb, sd_library, font_metrics)
        });
        esp_println::println!("bench: font_selftest ok={}", matches!(result, Ok(true)));
    }

    // A continuation is tiny Copy data, never a retained ZIP/FAT/parser borrow.
    // Give queued display/storage commands and the other executor tasks a turn
    // before appending the next spine during book preparation.
    let mut pending_reader: Option<StorageCommand> = None;
    // A loading plate can have the same request as its eventual page. Keep
    // this bit separate so a RAM reply only skips a proven body-page redraw.
    let mut last_reader_render_ready = false;
    // Whether the page on the panel holds images: a quick page turn out of
    // it would leave their dots' residue behind.
    let mut shown_has_images = false;
    let mut idle = IdlePreparation::new();
    loop {
        let idle_at = idle
            .target(
                sd_library,
                refresh_planner.last_request(),
                last_reader_render_ready,
                &sync_session,
            )
            .map(|_| idle.at);
        let wake = match (progress.deadline(), idle_at) {
            (Some(progress_at), Some(idle_at)) => Some(progress_at.min(idle_at)),
            (progress_at, idle_at) => progress_at.or(idle_at),
        };
        let incoming = if pending_reader.is_some() {
            match select(
                select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()),
                embassy_time::Timer::after_millis(1),
            )
            .await
            {
                Either::First(command) => Some(command),
                Either::Second(()) => Some(Either::Second(pending_reader.take().unwrap())),
            }
        } else if let Some(due) = wake {
            match select(
                select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()),
                embassy_time::Timer::at(due),
            )
            .await
            {
                Either::First(command) => Some(command),
                Either::Second(()) => None,
            }
        } else {
            Some(select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()).await)
        };
        let Some(incoming) = incoming else {
            let now = Instant::now();
            // A coalesced position's write window ended with no further
            // page turn: land it now rather than waiting for the next one.
            if progress.deadline().is_some_and(|due| due <= now) {
                progress.flush(&mut epd, &mut sd_cs, sd_library);
            }
            if idle_at.is_some_and(|at| at <= now) {
                prepare_while_idle(
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    font_metrics,
                    &mut epub_scratch,
                    refresh_planner.last_request(),
                    &mut last_reader_render_ready,
                    &sync_session,
                    &mut idle,
                );
            }
            continue;
        };
        // Any command means the reader is active again.
        idle.rearm(IDLE_PREPARATION_DELAY);
        match incoming {
            Either::First(DisplayCommand::Render(request)) => {
                if pending_reader.is_some_and(|command| !pending_reader_matches(command, request)) {
                    pending_reader = None;
                }
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
                            app_core::is_portrait(request.orientation),
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
                                    app_core::is_portrait(request.orientation),
                                );
                            }
                        }
                    }
                }
                let layout_start = Instant::now();
                if !render_reader_from_card(
                    &mut epd,
                    &mut sd_cs,
                    fb,
                    request,
                    sd_library,
                    font_metrics,
                    tx_band,
                ) {
                    crate::views::render(fb, request, sd_library);
                }
                let layout_ms = layout_start.elapsed().as_millis();

                if !panel_ready {
                    esp_println::println!("display: wake init start");
                    if let Err(error) = display_flush::init_panel(&mut epd).await {
                        esp_println::println!("display: wake init failed: {:?}", error);
                        prev_prestaged = false;
                        send_required_display_event(&DisplayEvent::Failed);
                        let _ = POWER_EVENTS.try_send(PowerEvent::DisplayFailed);
                        continue;
                    }
                    esp_println::println!("display: wake init complete");
                    prev_prestaged = false;
                    panel_ready = true;
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
                let has_images = crate::views::reader_page_has_images(request, sd_library);
                if display_flush::flush(
                    &mut epd,
                    fb,
                    prev_fb,
                    tx_band,
                    refresh_planner.screen_on(),
                    mode,
                    prev_prestaged,
                    !(has_images || shown_has_images),
                )
                .await
                .is_ok()
                {
                    shown_has_images = has_images;
                    let flush_ms = flush_start.elapsed().as_millis();
                    refresh_planner.record_render(request, mode);
                    last_reader_render_ready = request.view == AppView::Reading
                        && ReaderSource::from_book_id(request.book_id)
                            .sd_index()
                            .is_some_and(|index| {
                                sd_library.covers_global_page(index as usize, request.page)
                            })
                        && sd_library.type_settings() == request.type_settings()
                        && sd_library.portrait() == app_core::is_portrait(request.orientation);
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
                    #[cfg(feature = "bench-serial")]
                    if crate::tasks::bench_serial::FB_DUMP_ARMED
                        .load(core::sync::atomic::Ordering::Relaxed)
                    {
                        // No atomic swap on this core; a lost race only
                        // costs one dump.
                        crate::tasks::bench_serial::FB_DUMP_ARMED
                            .store(false, core::sync::atomic::Ordering::Relaxed);
                        crate::tasks::bench_serial::dump_framebuffer(fb).await;
                    }
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
                                page: request.page,
                            });
                        }
                    }
                    let (display_event, power_event) = app_core::display_refresh_outcome(true);
                    send_required_display_event(&display_event);
                    let _ = POWER_EVENTS.try_send(power_event);
                } else {
                    esp_println::println!("display: SPI transfer failed");
                    prev_prestaged = false;
                    let (display_event, power_event) = app_core::display_refresh_outcome(false);
                    send_required_display_event(&display_event);
                    let _ = POWER_EVENTS.try_send(power_event);
                }
            }
            Either::First(DisplayCommand::Sleep) => {
                pending_reader = None;
                let sleep_start = Instant::now();
                esp_println::println!(
                    "bench: sleep phase=requested screen_on={} t_ms={}",
                    refresh_planner.screen_on(),
                    sleep_start.as_millis(),
                );
                // Display commands are polled first, so a position queued just
                // ahead of this sleep may still sit in the storage queue.
                crate::stack_probe::report("sleep");
                fold_queued_progress(&mut epd, &mut sd_cs, sd_library, &mut progress);
                // One retry, then sleep regardless: refusing to sleep over a
                // failed write (a pulled card) only drained the battery, and
                // the position cannot reach the card either way.
                if !progress.flush(&mut epd, &mut sd_cs, sd_library)
                    && !progress.flush(&mut epd, &mut sd_cs, sd_library)
                {
                    esp_println::println!(
                        "display: sleeping without saving the reading position; card write failed"
                    );
                }
                let request = refresh_planner.last_request().or_else(|| {
                    sleep_request_from_saved_state(
                        &mut epd,
                        &mut sd_cs,
                        sd_library,
                        progress.pending(),
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
                    false,
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
                let panel_slept = display_flush::sleep_panel(&mut epd).await.is_ok();
                panel_ready = false;
                let ok = panel_slept && sleep_frame_settled;
                crate::wake_mark::set(ok);
                if ok {
                    refresh_planner.record_sleep();
                } else {
                    // The panel may hold a half-written frame or be powered
                    // down; if this sleep is aborted, the next render must
                    // re-initialise it rather than diff against a guess.
                    esp_println::println!(
                        "display: sleep transition incomplete (frame={} panel={})",
                        sleep_frame_settled,
                        panel_slept
                    );
                    refresh_planner.record_panel_unknown();
                }
                // Deep sleep proceeds either way: wake is a full reboot that
                // re-initialises the panel, and staying awake after a failed
                // transition only drains the battery.
                send_required_display_event(&DisplayEvent::Asleep);
                let _ = POWER_EVENTS.try_send(PowerEvent::DisplayAsleep);
                esp_println::println!(
                    "bench: sleep phase=complete ok={} elapsed_ms={} t_ms={}",
                    ok,
                    sleep_start.elapsed().as_millis(),
                    Instant::now().as_millis(),
                );
            }
            Either::Second(StorageCommand::ReceiveUpload) => {
                if sync_session.admits(&StorageCommand::ReceiveUpload) {
                    crate::sd_session::upload_session(&mut epd, &mut sd_cs).await;
                    continue;
                }
                esp_println::println!("storage: upload refused outside sync");
            }
            Either::Second(command) => {
                // Cancellation must win before even the loading plate: a stale
                // request must not repaint Reading over a newer Home frame.
                let stale = match command {
                    StorageCommand::OpenBook { request_id, .. }
                    | StorageCommand::ExtendSection { request_id, .. }
                    | StorageCommand::JumpChapter { request_id, .. }
                    | StorageCommand::LoadChapters { request_id, .. } => {
                        request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed)
                    }
                    _ => false,
                };
                if stale {
                    continue;
                }
                // Preparing a book (a first build, or a layout change that
                // re-paginates it) blocks this task for the whole rebuild.
                // Paint the title/author plate first so the wait reads as
                // loading, not frozen: the store still reports the old book
                // or settings here, so the reader view lands on the loading
                // branch. The app holds its render until the open completes,
                // so this is the only paint a slow open gets.
                if refresh_planner.screen_on() {
                    if let Some(loading_request) = open_loading_plate_request(
                        &command,
                        sd_library,
                        refresh_planner.last_request(),
                        last_reader_render_ready,
                    ) {
                        crate::views::render(fb, loading_request, sd_library);
                        let mode = refresh_planner.mode_for(loading_request);
                        let quick_allowed = !shown_has_images;
                        shown_has_images = false;
                        if display_flush::flush(
                            &mut epd,
                            fb,
                            prev_fb,
                            tx_band,
                            refresh_planner.screen_on(),
                            mode,
                            prev_prestaged,
                            quick_allowed,
                        )
                        .await
                        .is_ok()
                        {
                            refresh_planner.record_render(loading_request, mode);
                            last_reader_render_ready = false;
                            prev_fb.copy_from(fb);
                            prev_prestaged = false;
                        }
                    }
                }
                let reader_page_already_rendered = last_reader_render_ready
                    && refresh_planner.screen_on()
                    && refresh_planner.last_request().is_some_and(|request| {
                        matches!(command, StorageCommand::ExtendSection { target_pages, .. }
                            if u32::from(target_pages) == request.page)
                            && pending_reader_matches(command, request)
                    });
                handle_storage_command(
                    command,
                    reader_page_already_rendered,
                    &mut epd,
                    &mut sd_cs,
                    sd_library,
                    font_metrics,
                    &mut epub_scratch,
                    &mut sync_session,
                    &mut progress,
                    &mut state_restored,
                    &mut pending_reader,
                );
                crate::stack_probe::report("storage");
            }
        }
    }
}

pub(crate) fn pending_reader_matches(command: StorageCommand, request: RenderRequest) -> bool {
    let (book_id, type_settings, portrait) = match command {
        StorageCommand::OpenBook {
            book_id,
            type_settings,
            portrait,
            ..
        }
        | StorageCommand::ExtendSection {
            book_id,
            type_settings,
            portrait,
            ..
        }
        | StorageCommand::JumpChapter {
            book_id,
            type_settings,
            portrait,
            ..
        } => (book_id, type_settings, portrait),
        _ => return false,
    };
    request.view == AppView::Reading
        && request.book_id == book_id
        && request.type_settings() == type_settings
        && app_core::is_portrait(request.orientation) == portrait
}

/// Idle-time preparation of the rest of the open book: when it may next
/// run, the button-press count it was armed at (a press since then means
/// the reader is back), a spine that failed, not retried until the book's
/// preparation moves on some other way, and the image work done since the
/// reader last did anything.
struct IdlePreparation {
    at: Instant,
    presses: u32,
    /// The book's catalog index and the spine. The index is kept as a u16,
    /// as commands carry it, so `images` fits without growing the display
    /// task's static state.
    failed: Option<(u16, u16)>,
    images: IdleImageWork,
}

impl IdlePreparation {
    fn new() -> Self {
        Self {
            at: Instant::now() + IDLE_PREPARATION_DELAY,
            presses: BUTTON_PRESSES.load(Ordering::Relaxed),
            failed: None,
            images: IdleImageWork::default(),
        }
    }

    /// Any command: the reader is back, and may be on another page, so the
    /// window around it is looked at again unless the store's note covers
    /// it. What the note holds lasts.
    fn rearm(&mut self, delay: Duration) {
        self.resume(delay);
        self.images = IdleImageWork::default();
    }

    /// The next step after `delay`, picking up the image work where it is.
    fn resume(&mut self, delay: Duration) {
        self.at = Instant::now() + delay;
        self.presses = BUTTON_PRESSES.load(Ordering::Relaxed);
    }

    /// Text layout still to do, short of a spine that failed.
    fn layout_pending(&self, sd_library: &ReaderStore, index: usize) -> bool {
        sd_library
            .book_next_spine()
            .is_some_and(|next_spine| self.failed != Some((index as u16, next_spine)))
    }

    /// The page on screen and its book, while that book has preparation
    /// left (text layout, or images that may need decoding) and nothing
    /// else needs the card: the reader is on a resident page of it in the
    /// layout it was prepared for, and no sync session holds the scratch.
    fn target(
        &self,
        sd_library: &ReaderStore,
        last_request: Option<RenderRequest>,
        last_reader_render_ready: bool,
        sync_session: &SyncSession,
    ) -> Option<(RenderRequest, usize)> {
        let request = last_request?;
        let index = ReaderSource::from_book_id(request.book_id).sd_index()? as usize;
        let layout = self.layout_pending(sd_library, index);
        let images = self.images.next(sd_library, request.page, layout) != IdleImageStep::Nothing;
        ((layout || images)
            && last_reader_render_ready
            && !sync_session.active()
            && sd_library.loaded_index == Some(index)
            && sd_library.reader_status() == BookLoadStatus::Ready)
            .then_some((request, index))
    }
}

/// Prepare the open book's next spine while the reader is idle, leaving the
/// page on screen resident, and tell the app how far the book now reaches.
/// Then arm the next step: after a yield to the other tasks, or once the
/// reader is idle again if a press came in. Out of line, so the task loop's
/// poll frame stays small.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn prepare_while_idle(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    last_request: Option<RenderRequest>,
    last_reader_render_ready: &mut bool,
    sync_session: &SyncSession,
    idle: &mut IdlePreparation,
) {
    let Some((request, index)) = idle.target(
        sd_library,
        last_request,
        *last_reader_render_ready,
        sync_session,
    ) else {
        return;
    };
    let presses = idle.presses;
    if BUTTON_PRESSES.load(Ordering::Relaxed) != presses {
        idle.rearm(IDLE_PREPARATION_DELAY);
        return;
    }
    let scratch = ensure_epub_scratch(epub_scratch);
    let layout = idle.layout_pending(sd_library, index);
    // The pages around the one on screen first: their images are what the
    // next page turn needs. A step that decoded some ends there, so a step
    // stays within one session's budget, and so does one the budget cut
    // short, whose window the next step goes on with.
    if idle.images.next(sd_library, request.page, layout) == IdleImageStep::Window {
        let pass = ImagePass::Window;
        let end = reader_cache::decode_images_while_idle(
            epd,
            sd_cs,
            sd_library,
            index,
            request.page,
            scratch,
            pass,
            presses,
        );
        if end.pressed {
            idle.rearm(IDLE_PREPARATION_DELAY);
            return;
        }
        if end.finished {
            idle.images.window_tried = true;
        }
        if end.worked || !end.finished {
            idle.resume(Duration::from_millis(1));
            return;
        }
    }
    if !layout {
        // The rest of the book's images, once its text is laid out.
        if idle.images.next(sd_library, request.page, false) == IdleImageStep::Rest {
            let pass = ImagePass::Rest;
            let end = reader_cache::decode_images_while_idle(
                epd,
                sd_cs,
                sd_library,
                index,
                request.page,
                scratch,
                pass,
                presses,
            );
            if end.pressed {
                idle.rearm(IDLE_PREPARATION_DELAY);
                return;
            }
            // Through without settling the book and without decoding: a
            // card fault. Not again until the reader does something.
            if end.finished && !end.worked && !sd_library.book_images_settled() {
                idle.images.rest_stuck = true;
            }
        }
        crate::stack_probe::report("images");
        idle.resume(Duration::from_millis(1));
        return;
    }
    let started = Instant::now();
    let next_spine = sd_library.book_next_spine();
    reader_cache::build_or_load_book_cache(
        epd,
        sd_cs,
        sd_library,
        index,
        request.chapter,
        request.page as usize,
        None,
        scratch,
        font_metrics,
        Preparation::Idle { presses },
    );
    let pressed = BUTTON_PRESSES.load(Ordering::Relaxed) != presses;
    let grew = sd_library.book_next_spine() != next_spine;
    // The page on screen may have been numbered provisionally; once the
    // prefix reaches it, it takes its real number.
    let shown = sd_library.settled_page(request.page);
    esp_println::println!(
        "bench: idle_prepare elapsed_ms={} grew={} pressed={} next_spine={:?} pages={} status={:?} t_ms={}",
        started.elapsed().as_millis(),
        grew,
        pressed,
        sd_library.book_next_spine(),
        sd_library.advertised_page_count(),
        sd_library.reader_status(),
        Instant::now().as_millis(),
    );
    if (grew || shown != request.page) && sd_library.reader_status() == BookLoadStatus::Ready {
        send_loaded_library_event(&LibraryEvent::Prepared {
            book_id: request.book_id,
            pages: sd_library.advertised_page_count(),
            chapters: sd_library.chapter_count_for_ui(),
            chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
            complete: sd_library.book_next_spine().is_none(),
            renumbered: (shown != request.page).then_some((request.page, shown)),
        });
    }
    if !grew && !pressed {
        idle.failed = next_spine.map(|spine| (index as u16, spine));
    }
    if grew {
        // New pages around the reader, and the rest pass's to find.
        idle.images = IdleImageWork::default();
    }
    // The page on screen is read back after every step; say whether it is
    // still resident for the next request.
    *last_reader_render_ready = sd_library.covers_global_page(index, request.page);
    crate::stack_probe::report("idle");
    if pressed {
        idle.rearm(IDLE_PREPARATION_DELAY);
    } else {
        idle.resume(Duration::from_millis(1));
    }
}

/// Decodes the images the page about to be answered shows, for its
/// orientation (see `reader_cache::decode_page_images`). False when a press
/// since `presses`, or a newer request than `request_id`, abandoned the
/// wait. Out of line, so the storage dispatcher's frame, on the deepest
/// build path, does not carry it.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn page_images_ready(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    index: usize,
    page: u32,
    presses: u32,
    request_id: u32,
) -> bool {
    match epub_scratch.as_deref_mut() {
        Some(scratch) => reader_cache::decode_page_images(
            epd, sd_cs, sd_library, index, page, scratch, presses, request_id,
        ),
        None => true,
    }
}

fn send_library_event(event: &LibraryEvent) {
    if LIBRARY_EVENTS.try_send(*event).is_err() {
        esp_println::println!("display: library event queue full");
    }
}

/// Draws a reading page that needs the card, in one SD session: one set in
/// a custom face from the card, or one with images, whose rows stream from
/// the book's packs through `band`. The band is the panel transfer buffer:
/// flushes and prestages fill it a band at a time just before sending, so
/// nothing in it outlives a transfer, and it is idle while a frame draws.
/// False when the page needs no card, or the session cannot open; the
/// caller then draws it from RAM, with the built-in face and image
/// placeholders, and never leaves the previous frame up.
fn render_reader_from_card(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    band: &mut [u8; BAND_BYTES],
) -> bool {
    if !crate::views::reader_reads_card(request, sd_library) {
        return false;
    }
    crate::sd_session::with_root(epd, sd_cs, |root| {
        crate::views::render_reader_from_root(fb, request, sd_library, font_metrics, root, band)
    })
    .unwrap_or(false)
}

/// The reader-view render to paint as a loading plate before an open/extend
/// that needs book preparation. Prepared sections skip it even outside RAM,
/// and the page then arrives in a single refresh from the completion event;
/// painting here keeps a first cache build from looking frozen on the
/// previous screen.
fn open_loading_plate_request(
    command: &StorageCommand,
    sd_library: &ReaderStore,
    last_request: Option<RenderRequest>,
    last_reader_render_ready: bool,
) -> Option<RenderRequest> {
    let (book_id, index, target_pages, type_settings, portrait) = match *command {
        StorageCommand::OpenBook {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        StorageCommand::ExtendSection {
            book_id,
            index,
            target_pages,
            type_settings,
            portrait,
            ..
        } => (book_id, index, target_pages, type_settings, portrait),
        _ => return None,
    };
    // Only SD books re-paginate and route to the reader loading plate; the
    // built-in book renders from embedded content and never rebuilds.
    if !ReaderSource::from_book_id(book_id).is_sd() {
        return None;
    }
    if sd_library.type_settings() == type_settings
        && sd_library.portrait() == portrait
        && sd_library.has_prepared_page(index as usize, target_pages as u32)
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
    request.alignment = type_settings.alignment;
    // Continuation steps keep the same loading plate. Repainting it between
    // spines adds a full panel refresh to every preparation step. The plate
    // shows no page, so a resume whose target only resolved after the first
    // step (page 0, then the saved page) still counts as the same plate.
    if !last_reader_render_ready
        && last_request.is_some_and(|last| {
            RenderRequest {
                page: request.page,
                ..last
            } == request
        })
    {
        return None;
    }
    Some(request)
}

/// Kept out of line so the task loop's poll frame stays small; the storage
/// arms below carry multi-KB scratch and run near the stack floor.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn handle_storage_command(
    command: StorageCommand,
    reader_page_already_rendered: bool,
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch<'static>>,
    sync_session: &mut SyncSession,
    progress: &mut ProgressJournal,
    state_restored: &mut bool,
    pending_reader: &mut Option<StorageCommand>,
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
            *pending_reader = None;
            // The session only ends in a reset, so any coalesced position
            // must reach the card before the scratch is dismantled. A failed
            // write must not withhold the loan: the wifi task waits on it,
            // and the session's reset would lose the position regardless.
            if !progress.flush(epd, sd_cs, sd_library) {
                esp_println::println!("storage: sync loan proceeding; reading position not saved");
            }
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
                // With nothing restored, the title page defaults to the first
                // book; the cached load reads no list entries, so stage its
                // record for the label.
                if sd_library.active_index().is_none() && !sd_library.catalog_is_empty() {
                    crate::library_sd::load_active_entry(epd, sd_cs, sd_library, 0);
                }
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
            portrait,
        }
        | StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            type_settings,
            portrait,
        } => {
            *pending_reader = None;
            let storage_start = Instant::now();
            // A press from here on abandons a wait for the page's images.
            let presses = BUTTON_PRESSES.load(Ordering::Relaxed);
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
            // A coalesced position for the previous book locates that book's
            // POS file through the active entry this open is about to
            // replace; write it first or the switch silently drops it. A
            // resume of the same book reads its POS file too, so a position
            // still waiting out the write deadline lands first.
            let resumes = is_open_book && chapter == 0 && target_pages == 0;
            if progress
                .pending_book()
                .is_some_and(|pending| pending != book_id || resumes)
            {
                progress.flush(epd, sd_cs, sd_library);
            }
            // Read this book's catalog record into the active-entry slot so the
            // reader pipeline (load_position, build_or_load) resolves it from
            // the card rather than the list window. A failure leaves the entry
            // unset and the open falls through to the usual bad-index error.
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            // Adopt the command's type settings before the RAM fast path:
            // a settings change drops the loaded page coverage, so the
            // request falls through to the cache load/rebuild below.
            sd_library.set_layout(type_settings, portrait);
            // A fresh selection (chapter 0, page 0) resumes from the
            // book's own saved position; explicit page requests pass
            // through untouched. Extends never resume.
            let mut chapter = chapter;
            let mut target_pages = target_pages;
            let mut first_open = false;
            if is_open_book && chapter == 0 && target_pages == 0 {
                match reader_cache::load_position(epd, sd_cs, sd_library, index as usize) {
                    Some((saved_chapter, saved_screen)) => {
                        if saved_chapter > 0 || saved_screen > 0 {
                            chapter = saved_chapter;
                            target_pages = saved_screen.min(u16::MAX as u32) as u16;
                            esp_println::println!(
                                "storage: resume book {} at chapter {} screen {}",
                                book_id,
                                chapter,
                                target_pages
                            );
                        }
                    }
                    // Never read: open where the text starts, past the
                    // cover and front matter, once the book says where.
                    None => {
                        if let Some(entry) = sd_library.catalog_entry(index as usize) {
                            let source = (entry.source_hash, entry.byte_size);
                            sd_library.seek_text_start(source);
                            first_open = true;
                            esp_println::println!(
                                "storage: first open of book {}; seeking text start",
                                book_id
                            );
                        }
                    }
                }
            }
            // A book already in memory knows where its text starts.
            if first_open && sd_library.loaded_index == Some(index as usize) {
                if let Some(page) = sd_library.requested_page(0, None) {
                    target_pages = page.min(u16::MAX as u32) as u16;
                }
            }
            // A page numbered while its chapter was laid out ahead of the
            // rest takes its real number once the rest has caught up.
            if sd_library.loaded_index == Some(index as usize) {
                target_pages = sd_library
                    .settled_page(u32::from(target_pages))
                    .min(u16::MAX as u32) as u16;
            }
            // The requested page is usually inside the section window that
            // is already loaded; answering from RAM keeps ordinary page
            // turns free of card init, FAT, and cache-file traffic.
            if sd_library.covers_global_page(index as usize, target_pages as u32) {
                if !reader_page_already_rendered
                    && !page_images_ready(
                        epd,
                        sd_cs,
                        sd_library,
                        epub_scratch,
                        index as usize,
                        target_pages as u32,
                        presses,
                        request_id,
                    )
                {
                    sd_library.end_reader_request();
                    *pending_reader = command.retry_at_page(u32::from(target_pages));
                    return;
                }
                sd_library.end_reader_request();
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
                if reader_page_already_rendered {
                    // The input render already showed this RAM page. Keep the
                    // uncapped chapter cursor in sync without a second refresh.
                    send_loaded_library_event(&LibraryEvent::ChapterCursor {
                        book_id,
                        current_chapter: sd_library.current_chapter(),
                        page: target_pages as u32,
                    });
                    return;
                }
                send_loaded_library_event(&LibraryEvent::PrefixLoaded {
                    request_id,
                    book_id,
                    pages: sd_library.advertised_page_count(),
                    chapters: sd_library.chapter_count_for_ui(),
                    current_chapter: sd_library.current_chapter(),
                    chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                    complete: sd_library.book_next_spine().is_none(),
                    page: Some(target_pages as u32),
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
                None,
                scratch,
                font_metrics,
                Preparation::Request,
            );
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            if sd_library.reader_status() == BookLoadStatus::Loading {
                *pending_reader = Some(StorageCommand::ExtendSection {
                    request_id,
                    book_id,
                    index,
                    chapter,
                    target_pages,
                    type_settings,
                    portrait,
                });
                return;
            }
            // A finished load has spent any page pin it took; a later
            // request for the same page number means the new pagination.
            if sd_library.reader_status() == BookLoadStatus::Error {
                sd_library.end_reader_request();
                send_loaded_library_event(&LibraryEvent::ReaderLoadFailed {
                    request_id,
                    book_id,
                });
                return;
            }
            let page = sd_library.requested_page(target_pages as u32, None);
            if let Some(resolved) = page.filter(|&page| {
                !page_images_ready(
                    epd,
                    sd_cs,
                    sd_library,
                    epub_scratch,
                    index as usize,
                    page,
                    presses,
                    request_id,
                )
            }) {
                // Retried unless a newer request replaces it, and at the
                // page this one resolved to: the pins that resolved it (a
                // first open's text start, the place a relayout kept) are
                // spent here, and the page number asked for would land
                // elsewhere without them.
                sd_library.end_reader_request();
                *pending_reader = command.retry_at_page(resolved);
                return;
            }
            send_loaded_library_event(&LibraryEvent::PrefixLoaded {
                request_id,
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                complete: sd_library.book_next_spine().is_none(),
                page,
            });
            sd_library.end_reader_request();
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
            send_loaded_library_event(&LibraryEvent::PrefixLoaded {
                request_id,
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                complete: sd_library.book_next_spine().is_none(),
                page: None,
            });
        }
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            type_settings,
            portrait,
        } => {
            *pending_reader = None;
            let presses = BUTTON_PRESSES.load(Ordering::Relaxed);
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            crate::library_sd::load_active_entry(epd, sd_cs, sd_library, index as usize);
            sd_library.set_layout(type_settings, portrait);
            // The TOC is still in the buffer; resolve the chapter's start page
            // before loading the section overwrites it. Re-ensure the window
            // covers the selection in case it slid since the overview render.
            reader_cache::ensure_toc_window(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter as usize,
                portrait,
            );
            let requested_spine =
                u16::try_from(sd_library.overview_spine_at(chapter as usize)).ok();
            let target_page = requested_spine
                .and_then(|spine| sd_library.known_page_for_spine(spine))
                .unwrap_or(0);
            let scratch = ensure_epub_scratch(epub_scratch);
            reader_cache::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_page as usize,
                requested_spine,
                scratch,
                font_metrics,
                Preparation::Request,
            );
            if request_id != LATEST_READER_REQUEST_ID.load(Ordering::Relaxed) {
                return;
            }
            if sd_library.reader_status() == BookLoadStatus::Loading {
                *pending_reader = Some(command);
                return;
            }
            let page = sd_library.requested_page(target_page, requested_spine);
            if let Some(resolved) = page.filter(|&page| {
                sd_library.reader_status() != BookLoadStatus::Error
                    && !page_images_ready(
                        epd,
                        sd_cs,
                        sd_library,
                        epub_scratch,
                        index as usize,
                        page,
                        presses,
                        request_id,
                    )
            }) {
                // Retried at the chapter's page, as a request for it.
                sd_library.end_reader_request();
                *pending_reader = command.retry_at_page(resolved);
                return;
            }
            sd_library.end_reader_request();
            if sd_library.reader_status() == BookLoadStatus::Error {
                send_loaded_library_event(&LibraryEvent::ReaderLoadFailed {
                    request_id,
                    book_id,
                });
                return;
            }
            send_loaded_library_event(&LibraryEvent::PrefixLoaded {
                request_id,
                book_id,
                pages: sd_library.advertised_page_count(),
                chapters: sd_library.chapter_count_for_ui(),
                current_chapter: sd_library.current_chapter(),
                chapter_pages: crate::reader_store::chapter_pages_for_event(sd_library),
                complete: sd_library.book_next_spine().is_none(),
                page,
            });
        }
        StorageCommand::ReceiveUpload => {
            // Handled in the task loop before dispatch; reaching here means
            // the loop refused it already.
        }
        StorageCommand::StoreWifiCredentials(credentials) => {
            let record = hal_ext::nvm::WifiCredentialsRecord {
                ssid: credentials.ssid,
                ssid_len: credentials.ssid_len,
                password: credentials.password,
                password_len: credentials.password_len,
            };
            let written = reader_cache::store_wifi_credentials(epd, sd_cs, record);
            // Reacquire the card and use the exact boot-time read path before
            // telling the portal it may show success. This proves the file
            // survived handle/volume closure and closes the old reset race.
            let confirmed = written
                && reader_cache::load_wifi_credentials(epd, sd_cs)
                    .is_some_and(|stored| stored == record);
            esp_println::println!(
                "storage: wifi credentials written={} confirmed={}",
                written,
                confirmed
            );
            let _ = crate::WIFI_STORAGE_RESULTS.try_send(confirmed);
        }
        StorageCommand::ForgetWifiCredentials => {
            let forgotten = reader_cache::forget_wifi_credentials(epd, sd_cs);
            esp_println::println!("storage: wifi credentials forgotten={}", forgotten);
        }
        StorageCommand::ScanFirmwareFiles => {
            let count = crate::firmware_files::scan(epd, sd_cs, sd_library);
            esp_println::println!("storage: firmware files count={}", count);
            send_library_event(&LibraryEvent::FirmwareFiles { count });
        }
        StorageCommand::StageFirmwareUpdate { index } => {
            if !progress.flush(epd, sd_cs, sd_library) {
                esp_println::println!(
                    "storage: firmware stage deferred; progress persistence failed"
                );
                send_library_event(&LibraryEvent::FirmwareStageFailed);
                return;
            }
            if crate::firmware_files::stage(epd, sd_cs, sd_library, index as usize) {
                esp_println::println!("storage: firmware update staged index={}", index);
                esp_hal::system::software_reset();
            } else {
                esp_println::println!("storage: firmware update stage failed index={}", index);
                send_library_event(&LibraryEvent::FirmwareStageFailed);
            }
        }
        StorageCommand::StoreProgress(record) => {
            let record = progress_record(sd_library, record);
            progress.stage(epd, sd_cs, sd_library, record);
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

/// Builds the EPUB scratch into its static, once, from `main` before the
/// executor starts. Initialising it moves a ~21 KB value (mostly the inflate
/// state) through the stack. Done lazily at the first book open, as it used
/// to be, that spike sat on top of the display task and storage dispatcher
/// and was the deepest the stack ever went; at the base of the stack it
/// stays far below the reader's real call chains.
#[inline(never)]
pub(crate) fn init_epub_scratch() -> &'static mut ReaderCacheScratch<'static> {
    EPUB_SCRATCH.init_with(|| {
        ReaderCacheScratch::new(
            EPUB_TAIL.take(),
            EPUB_HEADER.take(),
            EPUB_NAME.take(),
            EPUB_COMPRESSED.take(),
            EPUB_CONTAINER.take(),
            EPUB_OPF.take(),
            EPUB_XHTML.take(),
        )
    })
}

/// The scratch is only ever absent while lent to a sync session, which
/// refuses reader work and ends in a reset.
fn ensure_epub_scratch<'a>(
    epub_scratch: &'a mut Option<&'static mut ReaderCacheScratch<'static>>,
) -> &'a mut ReaderCacheScratch<'static> {
    epub_scratch
        .as_deref_mut()
        .expect("EPUB scratch is lent only to the sync session")
}

fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    library.source_identity(book_id)
}

/// One boot-time attempt to map durable reader state back onto the scanned
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
        esp_println::println!("restore: no usable durable state");
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
    let page_count = restored_page_count(epd, sd_cs, usize::from(index), library, record.screen);
    send_required_library_event(&LibraryEvent::Restored {
        book_id: ReaderSource::sd(index).book_id(),
        chapter: record.chapter,
        page: record.screen,
        page_count,
        reading_orientation: record.reading_orientation,
        refresh_policy: record.refresh_policy,
        font_size: record.font_size,
        line_spacing: record.line_spacing,
        font_weight: record.font_weight,
        font_family: record.font_family,
        front_buttons: record.front_buttons,
        alignment: record.alignment,
    });
}

/// The saved book's page count for Home's progress, read from its index at
/// restore. A provisional page (laid out ahead of the rest of the book) has
/// no place in the total yet: 0 lets Home fall back rather than show it
/// full.
fn restored_page_count(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    index: usize,
    library: &ReaderStore,
    page: u32,
) -> u32 {
    if page >= crate::reader_store::PROVISIONAL_PAGE_FLOOR {
        0
    } else {
        reader_cache::restore_book_page_count(epd, sd_cs, index, library)
    }
}

fn sleep_request_from_saved_state(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    library: &mut ReaderStore,
    pending_progress: Option<AppStateRecord>,
) -> Option<RenderRequest> {
    let record = match pending_progress {
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
    let page_count = restored_page_count(epd, sd_cs, usize::from(index), library, record.screen);
    Some(RenderRequest {
        kind: RenderKind::Page,
        view: AppView::Home,
        page: record.screen,
        page_count,
        chapter: record.chapter,
        selection: 0,
        book_id: ReaderSource::sd(index).book_id(),
        orientation: display_orientation_from_u8(record.reading_orientation)
            .unwrap_or(DisplayOrientation::LandscapeButtonsBottom),
        front_buttons: app_core::front_buttons_from_u8(record.front_buttons)
            .unwrap_or(app_core::FrontButtons::PagesRight),
        refresh_policy: refresh_policy_from_u8(record.refresh_policy)
            .unwrap_or(app_core::RefreshPolicy::FastOnly),
        font_size: display::font::FontSize::from_u8(record.font_size)
            .unwrap_or(display::font::FontSize::Medium),
        line_spacing: display::font::LineSpacing::from_u8(record.line_spacing)
            .unwrap_or(display::font::LineSpacing::Normal),
        font_weight: display::font::FontWeight::from_u8(record.font_weight)
            .unwrap_or(display::font::FontWeight::Normal),
        font_family: display::font::FontFamily::from_u8(record.font_family)
            .unwrap_or(display::font::FontFamily::Literata),
        alignment: display::font::Alignment::from_u8(record.alignment)
            .unwrap_or(display::font::Alignment::Justified),
        last_button: None,
        aux_raw: 0,
        nav_raw: 0,
        page_raw: 0,
        battery_mv: 0,
        battery_percent: 100,
        library_count: library.catalog_count_u16(),
        firmware_count: library.firmware_files().len() as u16,
        firmware_status: app_core::FirmwareUpdateStatus::Scanning,
        sync_status: SyncStatus::NotConfigured,
        wifi_ssid: [0; 32],
        wifi_ssid_len: 0,
        dirty: display::Rect::FULL,
    })
}

/// The durable record for a reducer progress snapshot: the book's stable
/// source identity, and for the loaded SD book the firmware's uncapped
/// current chapter (the reducer's is capped at 128, so a deep position
/// would otherwise save, and later show, a stuck chapter).
fn progress_record(
    sd_library: &ReaderStore,
    record: app_core::PersistedAppState,
) -> AppStateRecord {
    let (source_hash, source_size) = source_identity(sd_library, record.book_id);
    // Resolve the chapter of the saved page itself, not whichever chapter the
    // tracker saw last: an earlier render (a provisional page 0 while an
    // open resumed elsewhere) can leave the tracker behind the saved page.
    let chapter = if ReaderSource::from_book_id(record.book_id).is_sd()
        && sd_library.loaded_index == ReaderStore::selected_book_index(record.book_id)
    {
        sd_library
            .chapter_for_page_of((source_hash, source_size), record.screen)
            .unwrap_or_else(|| sd_library.current_chapter())
    } else {
        record.chapter
    };
    AppStateRecord {
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
        front_buttons: record.front_buttons,
        alignment: record.alignment,
        source_hash,
        source_size,
    }
}

/// Fold progress writes still waiting in the storage queue into the journal
/// (the caller flushes it next); every other command is put back in order.
/// Out of line: its command buffer must not widen the task loop's frame.
#[inline(never)]
fn fold_queued_progress(
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &ReaderStore,
    progress: &mut ProgressJournal,
) {
    let mut kept: heapless::Vec<StorageCommand, 4> = heapless::Vec::new();
    while let Ok(command) = STORAGE_COMMANDS.try_receive() {
        match command {
            StorageCommand::StoreProgress(record) => {
                let record = progress_record(sd_library, record);
                if progress
                    .pending_book()
                    .is_some_and(|pending| pending != record.book_id)
                {
                    progress.flush(epd, sd_cs, sd_library);
                }
                progress.replace_pending(record);
            }
            other => {
                let _ = kept.push(other);
            }
        }
    }
    for command in kept {
        if STORAGE_COMMANDS.try_send(command).is_err() {
            esp_println::println!("display: storage command dropped before sleep");
        }
    }
}

/// Reading-position persistence: one owner for coalescing, the write
/// deadline, and retries. Same-book page turns coalesce to at most one write
/// per `PROGRESS_WRITE_MIN_SECS`; any other change (book, chapter, layout)
/// lands at once. A coalesced record is written when its window ends, and
/// on demand before sleep, a Wi-Fi loan, a firmware stage, or a book switch.
struct ProgressJournal {
    pending: Option<AppStateRecord>,
    last_write: Option<Instant>,
    /// When the pending record is next written (window end or retry).
    due_at: Option<Instant>,
}

impl ProgressJournal {
    const fn new() -> Self {
        Self {
            pending: None,
            last_write: None,
            due_at: None,
        }
    }

    fn pending(&self) -> Option<AppStateRecord> {
        self.pending
    }

    fn pending_book(&self) -> Option<u32> {
        self.pending.map(|record| record.book_id)
    }

    /// When the loop must wake to write the pending record, if any.
    fn deadline(&self) -> Option<Instant> {
        self.pending.and(self.due_at)
    }

    /// Supersede the pending record without writing (used while draining
    /// before sleep, which flushes right after).
    fn replace_pending(&mut self, record: AppStateRecord) {
        self.pending = Some(record);
        self.due_at.get_or_insert_with(Instant::now);
    }

    #[inline(never)]
    fn stage(
        &mut self,
        epd: &mut Epd,
        sd_cs: &mut Output<'static>,
        sd_library: &ReaderStore,
        record: AppStateRecord,
    ) {
        // Only a different book's pending position must land first; a
        // same-book record is simply superseded.
        if self
            .pending
            .is_some_and(|pending| pending.book_id != record.book_id)
            && !self.flush(epd, sd_cs, sd_library)
        {
            esp_println::println!("storage: progress context switch deferred after write failure");
            return;
        }
        let context_changed = self.pending.is_some_and(|pending| {
            AppStateRecord {
                screen: record.screen,
                ..pending
            } != record
        });
        let due = self
            .last_write
            .map(|written| written.elapsed().as_secs() >= PROGRESS_WRITE_MIN_SECS)
            .unwrap_or(true);
        if context_changed || due {
            self.write(epd, sd_cs, sd_library, record, "write");
        } else {
            self.pending = Some(record);
            self.due_at = Some(
                self.last_write.unwrap_or_else(Instant::now)
                    + embassy_time::Duration::from_secs(PROGRESS_WRITE_MIN_SECS),
            );
            esp_println::println!(
                "bench: storage_progress action=coalesce book_id={} page={} t_ms={}",
                record.book_id,
                record.screen,
                Instant::now().as_millis(),
            );
        }
    }

    /// Write the pending record now. True when nothing is left pending.
    #[inline(never)]
    fn flush(
        &mut self,
        epd: &mut Epd,
        sd_cs: &mut Output<'static>,
        sd_library: &ReaderStore,
    ) -> bool {
        match self.pending {
            Some(record) => self.write(epd, sd_cs, sd_library, record, "flush"),
            None => true,
        }
    }

    fn write(
        &mut self,
        epd: &mut Epd,
        sd_cs: &mut Output<'static>,
        sd_library: &ReaderStore,
        record: AppStateRecord,
        action: &str,
    ) -> bool {
        let start = Instant::now();
        let stored = reader_cache::store_app_state(epd, sd_cs, sd_library, record);
        if stored {
            self.pending = None;
            self.due_at = None;
            self.last_write = Some(Instant::now());
        } else {
            self.pending = Some(record);
            self.due_at =
                Some(Instant::now() + embassy_time::Duration::from_secs(PROGRESS_RETRY_SECS));
        }
        esp_println::println!(
            "bench: storage_progress action={} ok={} book_id={} page={} elapsed_ms={} t_ms={}",
            action,
            stored,
            record.book_id,
            record.screen,
            start.elapsed().as_millis(),
            Instant::now().as_millis(),
        );
        stored
    }
}
