use crate::display_flush::{self, Epd};
use crate::reader_cache::{self, ReaderCacheScratch};
use crate::reader_store::{LibraryScanStatus, ReaderStore};
use crate::{
    AppView, DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, RefreshPolicy,
    DISPLAY_COMMANDS, DISPLAY_EVENTS, LIBRARY_EVENTS, POWER_EVENTS,
};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::BAND_BYTES;
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;

const FAST_REFRESH_ENABLED: bool = true;
const FULL_REFRESH_INTERVAL: u8 = 8;
const INITIAL_SECTION_PAGES: usize = 5;
const SECTION_EXTEND_PAGES: usize = 5;
const SECTION_EXTEND_THRESHOLD: u32 = 2;

#[embassy_executor::task]
pub async fn run(mut epd: Epd, mut sd_cs: Output<'static>) {
    esp_println::println!("display: started");

    static FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let fb = FB.init(Framebuffer::new());
    static PREV_FB: static_cell::StaticCell<Framebuffer> = static_cell::StaticCell::new();
    let prev_fb = PREV_FB.init(Framebuffer::new());
    static TX_BAND: static_cell::StaticCell<[u8; BAND_BYTES]> = static_cell::StaticCell::new();
    let tx_band = TX_BAND.init([0; BAND_BYTES]);
    static EPUB_SCRATCH: static_cell::StaticCell<ReaderCacheScratch> =
        static_cell::StaticCell::new();
    let mut epub_scratch = None;

    esp_println::println!("display: init start");
    display_flush::init_panel(&mut epd).await;
    esp_println::println!("display: init complete");

    let mut screen_on = false;
    let mut fast_refreshes = 0u8;
    let mut sd_library = ReaderStore::new();
    let mut last_view: Option<AppView> = None;
    let mut last_book_id: Option<u32> = None;
    let mut last_request: Option<crate::RenderRequest> = None;
    loop {
        match DISPLAY_COMMANDS.receive().await {
            DisplayCommand::Render(request) => {
                let mut content_context_changed =
                    last_view != Some(request.view) || last_book_id != Some(request.book_id);
                if matches!(request.view, AppView::Home | AppView::Library)
                    && sd_library.status == LibraryScanStatus::NotScanned
                {
                    sd_library.status = LibraryScanStatus::Scanning;
                    crate::library_sd::scan_books(&mut epd, &mut sd_cs, &mut sd_library);
                    let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Scanned {
                        count: sd_library.count.min(u8::MAX as usize) as u8,
                    });
                    if let Some(record) = reader_cache::load_app_state(&mut epd, &mut sd_cs) {
                        if let Some(index) = sd_library.find_entry_by_state(
                            record.source_hash,
                            record.source_size,
                            record.book_id,
                        ) {
                            sd_library.set_current_index(index);
                            reader_cache::load_current_cover(
                                &mut epd,
                                &mut sd_cs,
                                &mut sd_library,
                                index,
                            );
                            let book_id = index as u32 + 2;
                            let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Restored {
                                book_id,
                                chapter: record.chapter.min(u8::MAX as u16) as u8,
                                page: record.screen,
                                reading_orientation: record.reading_orientation,
                                refresh_policy: record.refresh_policy,
                            });
                        }
                    }
                }
                if request.view == AppView::Reading && request.book_id >= 2 {
                    let index = request.book_id.saturating_sub(2) as usize;
                    let requested_chapter = request.chapter;
                    let needs_initial_load = sd_library.loaded_index != Some(index)
                        || sd_library.loaded_chapter != requested_chapter;
                    let needs_extension =
                        !needs_initial_load && should_extend_section(request.page, &sd_library);
                    if needs_initial_load || needs_extension {
                        let target_pages = if needs_extension {
                            request
                                .page
                                .saturating_add(SECTION_EXTEND_PAGES as u32)
                                .max(sd_library.page_count as u32 + SECTION_EXTEND_PAGES as u32)
                                as usize
                        } else {
                            INITIAL_SECTION_PAGES
                        };
                        let scratch = epub_scratch
                            .get_or_insert_with(|| EPUB_SCRATCH.init(ReaderCacheScratch::new()));
                        reader_cache::build_or_load_book_cache(
                            &mut epd,
                            &mut sd_cs,
                            &mut sd_library,
                            index,
                            requested_chapter,
                            target_pages,
                            scratch,
                        );
                        let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Loaded {
                            book_id: request.book_id,
                            pages: advertised_page_count(&sd_library),
                            chapters: sd_library.chapter_count_for_ui(),
                        });
                        crate::reader_store::publish_chapter_pages(request.book_id, &sd_library);
                        content_context_changed = true;
                    }
                }
                crate::views::render(fb, request, &sd_library);
                if request.view == AppView::Reading && request.book_id >= 2 {
                    let (source_hash, source_size) = source_identity(&sd_library, request.book_id);
                    reader_cache::store_app_state(
                        &mut epd,
                        &mut sd_cs,
                        AppStateRecord {
                            book_id: request.book_id,
                            chapter: request.chapter as u16,
                            screen: request.page,
                            shell_orientation: crate::DisplayOrientation::PortraitButtonsLeft as u8,
                            reading_orientation: request.orientation as u8,
                            refresh_policy: request.refresh_policy as u8,
                            source_hash,
                            source_size,
                        },
                    );
                }

                if !screen_on && last_request.is_none() {
                    esp_println::println!("display: wake init start");
                    display_flush::init_panel(&mut epd).await;
                    esp_println::println!("display: wake init complete");
                }

                let mode = refresh_mode(screen_on, fast_refreshes, request.refresh_policy);
                if content_context_changed {
                    esp_println::println!(
                        "display: context changed, refresh policy {:?} -> {:?}",
                        request.refresh_policy,
                        mode
                    );
                }
                if display_flush::flush(&mut epd, fb, prev_fb, tx_band, screen_on, mode)
                    .await
                    .is_ok()
                {
                    screen_on = true;
                    last_view = Some(request.view);
                    last_book_id = Some(request.book_id);
                    if mode == RefreshMode::Fast {
                        fast_refreshes = fast_refreshes.saturating_add(1);
                    } else {
                        fast_refreshes = 0;
                    }
                    prev_fb.copy_from(fb);
                    let _ = DISPLAY_EVENTS.try_send(DisplayEvent::Settled);
                    let _ = POWER_EVENTS.send(PowerEvent::DisplaySettled).await;
                } else {
                    esp_println::println!("display: SPI transfer failed");
                }
                last_request = Some(request);
            }
            DisplayCommand::Sleep => {
                if let Some(request) = last_request {
                    crate::views::render_sleep(fb, request, &sd_library);
                    let _ = display_flush::flush(
                        &mut epd,
                        fb,
                        prev_fb,
                        tx_band,
                        screen_on,
                        RefreshMode::Full,
                    )
                    .await;
                    prev_fb.copy_from(fb);
                }
                if display_flush::sleep_panel(&mut epd).await.is_ok() {
                    screen_on = false;
                    fast_refreshes = 0;
                    last_view = None;
                    last_book_id = None;
                    last_request = None;
                    let _ = POWER_EVENTS.send(PowerEvent::DisplayAsleep).await;
                } else {
                    esp_println::println!("display: sleep command failed");
                    let _ = POWER_EVENTS.send(PowerEvent::DisplayAsleep).await;
                }
            }
        }
    }
}

fn should_extend_section(requested_page: u32, library: &ReaderStore) -> bool {
    library.section_partial
        && library.page_count > 0
        && requested_page.saturating_add(SECTION_EXTEND_THRESHOLD) >= library.page_count as u32
}

fn advertised_page_count(library: &ReaderStore) -> u32 {
    let cached = library.page_count.max(1) as u32;
    if library.section_partial {
        cached.saturating_add(1)
    } else {
        cached
    }
}

fn source_identity(library: &ReaderStore, book_id: u32) -> (u32, u32) {
    let Some(index) = book_id.checked_sub(2).map(|index| index as usize) else {
        return (0, 0);
    };
    if index >= library.count {
        return (0, 0);
    }
    let Some(entry) = library.entries.get(index) else {
        return (0, 0);
    };
    (entry.source_hash, entry.byte_size)
}

fn refresh_mode(screen_on: bool, fast_refreshes: u8, refresh_policy: RefreshPolicy) -> RefreshMode {
    if !FAST_REFRESH_ENABLED || !screen_on {
        return RefreshMode::Full;
    }
    match refresh_policy {
        RefreshPolicy::FastOnly | RefreshPolicy::FullOnWake => RefreshMode::Fast,
        RefreshPolicy::FullEveryTen if fast_refreshes >= FULL_REFRESH_INTERVAL => RefreshMode::Full,
        RefreshPolicy::FullEveryTen => RefreshMode::Fast,
    }
}
