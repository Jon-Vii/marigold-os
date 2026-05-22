use crate::display_flush::{self, Epd};
use crate::reader_cache::{self, ReaderCacheScratch};
use crate::reader_store::{BookLoadStatus, ReaderStore};
use crate::{
    AppView, DisplayCommand, DisplayEvent, LibraryEvent, PowerEvent, RefreshPolicy, StorageCommand,
    DISPLAY_COMMANDS, DISPLAY_EVENTS, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::BAND_BYTES;
use embassy_futures::select::{select, Either};
use esp_hal::gpio::Output;
use hal_ext::nvm::AppStateRecord;

const FAST_REFRESH_ENABLED: bool = true;
const FULL_REFRESH_INTERVAL: u8 = 8;
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
        match select(DISPLAY_COMMANDS.receive(), STORAGE_COMMANDS.receive()).await {
            Either::First(DisplayCommand::Render(request)) => {
                let content_context_changed =
                    last_view != Some(request.view) || last_book_id != Some(request.book_id);
                crate::views::render(fb, request, &sd_library);

                if !screen_on && last_request.is_none() {
                    esp_println::println!("display: wake init start");
                    display_flush::init_panel(&mut epd).await;
                    esp_println::println!("display: wake init complete");
                }

                let mode = if content_context_changed
                    || needs_clean_selection_refresh(request, last_request)
                    || needs_clean_library_refresh(request, last_request)
                {
                    RefreshMode::Full
                } else {
                    refresh_mode(screen_on, fast_refreshes, request.refresh_policy)
                };
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
            Either::First(DisplayCommand::Sleep) => {
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
                    let _ = DISPLAY_EVENTS.try_send(DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.send(PowerEvent::DisplayAsleep).await;
                } else {
                    esp_println::println!("display: sleep command failed");
                    let _ = DISPLAY_EVENTS.try_send(DisplayEvent::Asleep);
                    let _ = POWER_EVENTS.send(PowerEvent::DisplayAsleep).await;
                }
            }
            Either::Second(command) => {
                handle_storage_command(
                    command,
                    &mut epd,
                    &mut sd_cs,
                    &mut sd_library,
                    &mut epub_scratch,
                    || EPUB_SCRATCH.init(ReaderCacheScratch::new()),
                );
            }
        }
    }
}

fn handle_storage_command(
    command: StorageCommand,
    epd: &mut Epd,
    sd_cs: &mut Output<'static>,
    sd_library: &mut ReaderStore,
    epub_scratch: &mut Option<&'static mut ReaderCacheScratch>,
    init_scratch: impl FnOnce() -> &'static mut ReaderCacheScratch,
) {
    match command {
        StorageCommand::RefreshCatalog => {
            if sd_library.count == 0
                && crate::library_sd::load_catalog_cache(epd, sd_cs, sd_library)
            {
                let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Scanned {
                    count: sd_library.count.min(u8::MAX as usize) as u8,
                });
            }
            crate::library_sd::scan_books(epd, sd_cs, sd_library);
            let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Scanned {
                count: sd_library.count.min(u8::MAX as usize) as u8,
            });
        }
        StorageCommand::OpenBook {
            book_id,
            index,
            chapter,
            target_pages,
        }
        | StorageCommand::ExtendSection {
            book_id,
            index,
            chapter,
            target_pages,
        } => {
            sd_library.reader_status = BookLoadStatus::Loading;
            let scratch = epub_scratch.get_or_insert_with(init_scratch);
            reader_cache::build_or_load_book_cache(
                epd,
                sd_cs,
                sd_library,
                index as usize,
                chapter,
                target_pages as usize,
                scratch,
            );
            let _ = LIBRARY_EVENTS.try_send(LibraryEvent::Loaded {
                book_id,
                pages: advertised_page_count(sd_library),
                chapters: sd_library.chapter_count_for_ui(),
            });
            crate::reader_store::publish_chapter_pages(book_id, sd_library);
        }
        StorageCommand::StoreProgress(record) => {
            let (source_hash, source_size) = source_identity(sd_library, record.book_id);
            reader_cache::store_app_state(
                epd,
                sd_cs,
                AppStateRecord {
                    book_id: record.book_id,
                    chapter: record.chapter,
                    screen: record.screen,
                    shell_orientation: record.shell_orientation,
                    reading_orientation: record.reading_orientation,
                    refresh_policy: record.refresh_policy,
                    source_hash,
                    source_size,
                },
            );
        }
    }
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

fn needs_clean_selection_refresh(
    request: crate::RenderRequest,
    last_request: Option<crate::RenderRequest>,
) -> bool {
    let Some(last) = last_request else {
        return false;
    };
    if request.view != last.view || request.book_id != last.book_id {
        return false;
    }
    matches!(
        request.view,
        AppView::Library | AppView::Chapters | AppView::Settings
    ) && request.selection != last.selection
}

fn needs_clean_library_refresh(
    request: crate::RenderRequest,
    last_request: Option<crate::RenderRequest>,
) -> bool {
    let Some(last) = last_request else {
        return false;
    };
    request.view == AppView::Library && request.library_count != last.library_count
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
