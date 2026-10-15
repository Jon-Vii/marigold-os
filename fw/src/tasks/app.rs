use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, ReaderSource,
    RenderKind, StorageCommand, SyncCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS,
    LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS, SYNC_COMMANDS,
    SYNC_EVENTS,
};
use app_core::{AppView, ReaderState, ReducerContext, SyncStatus};
use core::sync::atomic::Ordering;
use embassy_futures::select::{select4, Either4};
use embassy_time::{Duration, Instant};

const POST_OPEN_CONFIRM_BLOCK_MS: u64 = 700;

#[embassy_executor::task]
pub async fn run() {
    esp_println::println!("app: started");
    let ctx = reducer_context();
    let mut state = ReaderState::boot();
    // Compile-time dev credentials name the network immediately; the
    // display task's boot probe of /XTEINK/WIFI.BIN arrives later and
    // overrides, matching the wifi task's stored-beats-built-in order.
    if let Some((ssid, _)) = crate::tasks::wifi::credentials() {
        if let Some(ssid) = app_core::WifiSsid::new(ssid) {
            state = state.apply_sync_event(app_core::SyncEvent::NetworkSaved(ssid));
        }
    }
    let mut rendering = false;
    let mut render_pending = false;
    let mut catalog_refresh_requested = true;
    let mut pending_storage: Option<StorageCommand> = None;
    // The newest position write the storage queue could not take yet. Kept
    // apart from `pending_storage` so a deferred open or forget can never
    // overwrite it, and replaced (never queued behind) by a newer one.
    let mut pending_progress: Option<StorageCommand> = None;
    // Type settings changed while away from Reading: the loaded section is
    // paginated under the old layout, so the next entry into Reading must
    // send an extend even though page and chapter are unchanged.
    let mut reader_relayout_pending = false;
    let mut opening_book: Option<u32> = None;
    // Opening a book or jumping to a chapter has a provisional position until
    // storage resolves it. Retain this marker across cancellation/failure so
    // another Home action cannot overwrite the unread saved position either.
    let mut unresolved_position: Option<u32> = None;
    let mut suppress_input_until_open_settled = false;
    let mut block_confirm_until: Option<Instant> = None;
    // Defer the first paint. ReaderState::boot() defaults to the built-in guide
    // (book_id 1), so drawing it now flashes an empty title page until the saved
    // book loads from SD ~1.5s later. Instead, kick the catalog + saved-position
    // restore now and leave the retained image (the sleep screen, on a deep-sleep
    // wake) up until it resolves; the first render is sent when Restored/Scanned
    // lands (see first_render_kind), so wake is a single refresh straight onto the
    // restored book. Now that sleep is terminal, wake is a full reboot — before
    // that this cold-boot path never showed, the real book stayed resident.
    let mut boot_render_pending = true;
    if STORAGE_COMMANDS
        .try_send(StorageCommand::LoadCatalogCache)
        .is_err()
    {
        esp_println::println!("app: storage queue full for catalog cache");
    }

    loop {
        let incoming = select4(
            INPUT_EVENTS.receive(),
            DISPLAY_EVENTS.receive(),
            LIBRARY_EVENTS.receive(),
            SYNC_EVENTS.receive(),
        )
        .await;
        // Library events arrive on two channels (inline with display events
        // to keep their order relative to renders, or on their own queue);
        // both take the same path.
        let library_event = match incoming {
            Either4::Second(DisplayEvent::Library(event)) | Either4::Third(event) => Some(event),
            _ => None,
        };
        if let Some(event) = library_event {
            if !library_event_is_current(&event) {
                continue;
            }
            if let Some(book_id) = loaded_book_id(&event) {
                if opening_book == Some(book_id) {
                    opening_book = None;
                }
            }
            let should_render = if boot_render_pending {
                library_event_allows_first_render(&event)
            } else {
                library_event_affects_view(&state, &event)
            };
            let previous_persisted = state.persisted();
            state = state.apply_library_event(ctx, event);
            let resolved_position = resolve_open_position(&event, &state, &mut unresolved_position);
            if matches!(event, crate::LibraryEvent::PrefixLoaded { .. })
                && unresolved_position != Some(state.book_id)
                && (resolved_position || previous_persisted != state.persisted())
            {
                queue_progress(
                    StorageCommand::StoreProgress(state.persisted()),
                    &mut pending_progress,
                );
            }
            if !should_render {
                continue;
            }
            if rendering {
                render_pending = true;
            } else {
                send_render(first_render_kind(&mut boot_render_pending), &state).await;
                rendering = true;
                render_pending = false;
            }
            continue;
        }
        match incoming {
            Either4::First(event) => {
                if matches!(event, InputEvent::Sample { button: None, .. }) {
                    // A button-less sample is a pure battery reading (the input
                    // task emits one at boot, before the first paint). Fold the
                    // charge into state but spend no panel refresh on it -- the
                    // value rides out on the next real paint. At boot that's the
                    // deferred Restored paint (see boot_render_pending), so the
                    // first screen shows the true charge instead of boot()'s
                    // 100% placeholder.
                    state = state.apply_input(ctx, event);
                    continue;
                }
                if matches!(
                    event,
                    InputEvent::Sample {
                        button: Some(Button::Power),
                        ..
                    }
                ) {
                    // Hand off to the power task, which drives the display to its
                    // sleep image and then deep-sleeps the SoC with the Power
                    // button armed as the wake source. Waking is a fresh boot
                    // (deep sleep is terminal), so there is no in-app "asleep"
                    // state to toggle back out of here.
                    esp_println::println!("app: sleep requested");
                    next_reader_request_id();
                    state.pending_page = None;
                    pending_storage = pending_storage
                        .filter(|command| !should_send_storage_immediately(*command));
                    opening_book = None;
                    suppress_input_until_open_settled = false;
                    // A position the queue could not take yet would otherwise
                    // only leave on the next Settled, which never comes before
                    // deep sleep. Hand it over first; the display task folds
                    // it into the write it makes before sleeping.
                    if let Some(command) = pending_progress.take() {
                        STORAGE_COMMANDS.send(command).await;
                    }
                    let _ = POWER_EVENTS.send(PowerEvent::SleepNow).await;
                    continue;
                }

                if state.view == AppView::Reading
                    && should_block_post_open_confirm(event, &mut block_confirm_until)
                {
                    esp_println::println!("app: confirm ignored after book open");
                    continue;
                }

                if opening_book.is_some() || suppress_input_until_open_settled {
                    // Respect the oriented Back key even while a cold open is
                    // parsing. The request generation cancels its storage work.
                    if state.view != AppView::Reading
                        || state.apply_input(ctx, event).view != AppView::Home
                    {
                        esp_println::println!("app: input ignored while book open pending");
                        continue;
                    }
                }

                let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                let previous = state;
                let previous_persisted = state.persisted();
                state = state.apply_input(ctx, event);
                if previous.view == AppView::Reading && state.view != AppView::Reading {
                    next_reader_request_id();
                    pending_storage = pending_storage
                        .filter(|command| !should_send_storage_immediately(*command));
                    opening_book = None;
                    suppress_input_until_open_settled = false;
                    block_confirm_until = None;
                }
                let next_persisted = state.persisted();
                if previous.type_settings() != state.type_settings()
                    || app_core::is_portrait(previous.orientation)
                        != app_core::is_portrait(state.orientation)
                {
                    reader_relayout_pending = true;
                }
                let mut storage_command = storage_command_for_transition(&previous, &state);
                if storage_command.is_none()
                    && reader_relayout_pending
                    && state.view == AppView::Reading
                {
                    if let Some(index) = ReaderSource::from_book_id(state.book_id).sd_index() {
                        storage_command = Some(extend_section_command(&state, index));
                    }
                }
                if storage_command.is_some() {
                    // Open/extend commands carry the current type settings,
                    // so any dispatched command syncs the reader store.
                    reader_relayout_pending = false;
                }
                // Keep the current page while its replacement is loaded, then
                // render once from the completion event. Rendering immediately
                // on section boundaries paints a loading plate before the short
                // cache read and adds redundant refreshes to subsequent turns.
                // Only defer when storage accepted the command: a full queue
                // still needs a render's Settled event to drain pending_storage.
                let mut awaiting_reader_data = false;
                if let Some(command) = storage_command {
                    if let Some(book_id) = provisional_position_book_id(command) {
                        unresolved_position = Some(book_id);
                    }
                    if should_send_storage_immediately(command) {
                        log_storage_command("send", command);
                        if let Some(book_id) = open_book_id(command) {
                            opening_book = Some(book_id);
                            suppress_input_until_open_settled = true;
                        }
                        if STORAGE_COMMANDS.try_send(command).is_err() {
                            log_storage_command("queue", command);
                            pending_storage = Some(command);
                        } else if matches!(
                            command,
                            StorageCommand::OpenBook { .. }
                                | StorageCommand::LoadChapters { .. }
                                | StorageCommand::ExtendSection { .. }
                        ) {
                            awaiting_reader_data = true;
                        }
                    } else {
                        log_storage_command("queue", command);
                        if let Some(book_id) = open_book_id(command) {
                            opening_book = Some(book_id);
                            suppress_input_until_open_settled = true;
                        }
                        pending_storage = Some(command);
                    }
                }
                if previous_persisted != next_persisted
                    && unresolved_position != Some(state.book_id)
                {
                    queue_progress(
                        StorageCommand::StoreProgress(next_persisted),
                        &mut pending_progress,
                    );
                }
                if let Some(command) = forget_command_for_transition(&previous, &state) {
                    log_storage_command("send", command);
                    if STORAGE_COMMANDS.try_send(command).is_err() && pending_storage.is_none() {
                        pending_storage = Some(command);
                    }
                }
                if let Some(command) = sync_command_for_transition(&previous, &state) {
                    esp_println::println!("app: sync command {:?}", command);
                    if SYNC_COMMANDS.try_send(command).is_err() {
                        esp_println::println!("app: sync command queue full");
                    }
                }
                // Reader commands render once, from their completion event,
                // rather than painting the provisional state now. A book
                // opened from the Library carries page 0 until storage reads
                // its saved position, so an immediate paint showed the first
                // page before the right one. Storage paints the loading plate
                // itself whenever the open, extend, or jump needs preparation
                // (see open_loading_plate_request), so a cache miss that
                // rebuilds for a minute still never looks frozen.
                if awaiting_reader_data {
                    render_pending = false;
                } else if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either4::Second(event) => match event {
                DisplayEvent::Settled => {
                    rendering = false;
                    if !catalog_refresh_requested {
                        catalog_refresh_requested = true;
                        if STORAGE_COMMANDS
                            .try_send(StorageCommand::LoadCatalogCache)
                            .is_err()
                        {
                            esp_println::println!("app: storage queue full for catalog cache");
                        }
                    }
                    if let Some(command) = pending_storage.take() {
                        log_storage_command("send", command);
                        if let Some(book_id) = provisional_position_book_id(command) {
                            unresolved_position = Some(book_id);
                        }
                        if let Some(book_id) = open_book_id(command) {
                            opening_book = Some(book_id);
                            suppress_input_until_open_settled = true;
                        }
                        STORAGE_COMMANDS.send(command).await;
                    }
                    if let Some(command) = pending_progress.take() {
                        STORAGE_COMMANDS.send(command).await;
                    }
                    if render_pending {
                        send_render(RenderKind::Page, &state).await;
                        rendering = true;
                        render_pending = false;
                    } else if suppress_input_until_open_settled && opening_book.is_none() {
                        suppress_input_until_open_settled = false;
                        block_confirm_until = Some(
                            Instant::now() + Duration::from_millis(POST_OPEN_CONFIRM_BLOCK_MS),
                        );
                    }
                }
                DisplayEvent::Asleep => {
                    esp_println::println!("app: display asleep");
                    rendering = false;
                    render_pending = false;
                    opening_book = None;
                    suppress_input_until_open_settled = false;
                    block_confirm_until = None;
                }
                DisplayEvent::Failed => {
                    esp_println::println!("app: display transition failed");
                    rendering = false;
                    render_pending = false;
                }
                // Routed through the shared library-event handler above.
                DisplayEvent::Library(_) => {}
            },
            Either4::Third(_) => {}
            Either4::Fourth(event) => {
                state = state.apply_sync_event(event);
                if state.view != AppView::Wireless {
                    continue;
                }
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, &state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
        }
    }
}

/// The first paint after boot uses `RenderKind::Boot` — a full refresh that
/// re-initialises the panel from its post-deep-sleep off state. Every paint
/// after that is an ordinary page. Consumes the one-shot flag.
fn first_render_kind(boot_render_pending: &mut bool) -> RenderKind {
    if core::mem::take(boot_render_pending) {
        RenderKind::Boot
    } else {
        RenderKind::Page
    }
}

fn library_event_is_current(event: &crate::LibraryEvent) -> bool {
    event
        .reader_request_id()
        .is_none_or(|request_id| request_id == LATEST_READER_REQUEST_ID.load(Ordering::Relaxed))
}

fn resolve_open_position(
    event: &crate::LibraryEvent,
    state: &ReaderState,
    unresolved: &mut Option<u32>,
) -> bool {
    if let crate::LibraryEvent::PrefixLoaded {
        book_id,
        page: Some(page),
        ..
    } = *event
    {
        if *unresolved == Some(book_id)
            && state.book_id == book_id
            && state.view == AppView::Reading
            && state.page == page
        {
            *unresolved = None;
            return true;
        }
    }
    false
}

fn library_event_affects_view(state: &ReaderState, event: &crate::LibraryEvent) -> bool {
    match *event {
        crate::LibraryEvent::Scanned { count } => {
            state.view == AppView::Library && state.library_count != count
        }
        crate::LibraryEvent::Loaded {
            book_id,
            pages: _,
            chapters: _,
            current_chapter: _,
            chapter_pages: _,
        } => state.book_id == book_id,
        crate::LibraryEvent::PrefixLoaded {
            book_id,
            page,
            complete,
            ..
        } => {
            state.book_id == book_id
                && (complete
                    || state.view == AppView::Chapters
                    || (state.view == AppView::Reading && page.is_some()))
        }
        // Idle-time growth changes nothing on the page being read.
        crate::LibraryEvent::Prepared { .. } => false,
        crate::LibraryEvent::ReaderLoadFailed { book_id, .. } => {
            state.book_id == book_id && state.view == AppView::Reading
        }
        crate::LibraryEvent::ChapterPage {
            book_id,
            chapter,
            page,
        } => {
            state.book_id == book_id
                && state
                    .sd_chapter_pages
                    .get(chapter as usize)
                    .map(|stored| *stored != page.min(u16::MAX as u32) as u16)
                    .unwrap_or(false)
        }
        // The reducer adopts the new chapter without a repaint (Reading shows
        // page-within-chapter, not the chapter), so it never forces a render.
        crate::LibraryEvent::ChapterCursor { .. } => false,
        crate::LibraryEvent::CustomFont { .. } => state.view == AppView::Settings,
        crate::LibraryEvent::FirmwareFiles { .. } | crate::LibraryEvent::FirmwareStageFailed => {
            state.view == AppView::FirmwareUpdate
        }
        crate::LibraryEvent::Restored { .. } => true,
    }
}

fn library_event_allows_first_render(event: &crate::LibraryEvent) -> bool {
    matches!(
        event,
        crate::LibraryEvent::Restored { .. } | crate::LibraryEvent::Scanned { .. }
    )
}

/// Send a position write now, or park it as the newest pending one. A
/// successful send retires any older parked write so it can never land
/// after (and overwrite) this newer position.
fn queue_progress(command: StorageCommand, pending_progress: &mut Option<StorageCommand>) {
    if STORAGE_COMMANDS.try_send(command).is_ok() {
        *pending_progress = None;
    } else {
        *pending_progress = Some(command);
    }
}

fn should_send_storage_immediately(command: StorageCommand) -> bool {
    matches!(
        command,
        StorageCommand::OpenBook { .. }
            | StorageCommand::ExtendSection { .. }
            | StorageCommand::LoadChapters { .. }
            | StorageCommand::JumpChapter { .. }
    )
}

fn provisional_position_book_id(command: StorageCommand) -> Option<u32> {
    match command {
        StorageCommand::OpenBook { book_id, .. } | StorageCommand::JumpChapter { book_id, .. } => {
            Some(book_id)
        }
        _ => None,
    }
}

fn open_book_id(command: StorageCommand) -> Option<u32> {
    match command {
        StorageCommand::OpenBook { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn loaded_book_id(event: &crate::LibraryEvent) -> Option<u32> {
    match *event {
        crate::LibraryEvent::Loaded { book_id, .. } => Some(book_id),
        crate::LibraryEvent::ReaderLoadFailed { book_id, .. } => Some(book_id),
        crate::LibraryEvent::PrefixLoaded {
            book_id,
            page: Some(_),
            ..
        } => Some(book_id),
        _ => None,
    }
}

fn should_block_post_open_confirm(event: InputEvent, block_until: &mut Option<Instant>) -> bool {
    let Some(until) = *block_until else {
        return false;
    };
    if Instant::now() >= until {
        *block_until = None;
        return false;
    }
    matches!(
        event,
        InputEvent::Sample {
            button: Some(Button::Confirm),
            ..
        }
    )
}

async fn send_render(kind: RenderKind, state: &ReaderState) {
    DISPLAY_COMMANDS
        .send(DisplayCommand::Render(state.render_request(kind)))
        .await;
}

fn log_storage_command(label: &str, command: StorageCommand) {
    match command {
        StorageCommand::OpenBook {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            ..
        } => esp_println::println!(
            "app: storage {label} open request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages}"
        ),
        StorageCommand::ExtendSection {
            request_id,
            book_id,
            index,
            chapter,
            target_pages,
            ..
        } => esp_println::println!(
            "app: storage {label} extend request={request_id} book_id={book_id} index={index} chapter={chapter} target={target_pages}"
        ),
        StorageCommand::StoreProgress(_) => {
            esp_println::println!("app: storage {label} progress")
        }
        StorageCommand::LoadCatalogCache => {
            esp_println::println!("app: storage {label} load catalog cache")
        }
        StorageCommand::RefreshCatalog => {
            esp_println::println!("app: storage {label} refresh catalog")
        }
        StorageCommand::LoanSyncMemory => {
            esp_println::println!("app: storage {label} loan sync memory")
        }
        StorageCommand::StoreWifiCredentials(_) => {
            esp_println::println!("app: storage {label} wifi credentials")
        }
        StorageCommand::ForgetWifiCredentials => {
            esp_println::println!("app: storage {label} forget wifi credentials")
        }
        StorageCommand::ReceiveUpload => {
            esp_println::println!("app: storage {label} receive upload")
        }
        StorageCommand::ScanFirmwareFiles => {
            esp_println::println!("app: storage {label} scan firmware files")
        }
        StorageCommand::StageFirmwareUpdate { index } => {
            esp_println::println!("app: storage {label} stage firmware index={index}")
        }
        StorageCommand::LoadChapters {
            request_id,
            book_id,
            index,
        } => esp_println::println!(
            "app: storage {label} load chapters request={request_id} book_id={book_id} index={index}"
        ),
        StorageCommand::JumpChapter {
            request_id,
            book_id,
            index,
            chapter,
            ..
        } => esp_println::println!(
            "app: storage {label} jump chapter request={request_id} book_id={book_id} index={index} chapter={chapter}"
        ),
    }
}

fn reducer_context() -> ReducerContext {
    ReducerContext::new(catalog::book_count(), catalog::chapter_count())
}

/// Confirm on the Wireless screen arms `Starting`; leaving the screen after
/// the radio ran has to reset the device because the loaned memory can
/// never come back.
fn sync_command_for_transition(previous: &ReaderState, next: &ReaderState) -> Option<SyncCommand> {
    if previous.sync_status != SyncStatus::Starting && next.sync_status == SyncStatus::Starting {
        return Some(SyncCommand::Start);
    }
    if previous.view == AppView::Wireless && next.view != AppView::Wireless {
        let radio_ran = !matches!(
            previous.sync_status,
            SyncStatus::NotConfigured | SyncStatus::Idle | SyncStatus::ForgetPending
        );
        if radio_ran {
            return Some(SyncCommand::Exit);
        }
    }
    None
}

/// Confirming the pending forget on the Wireless screen deletes the saved
/// credentials from the card. Both states live before any radio work, so
/// the storage path is still whole.
fn forget_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    (previous.sync_status == SyncStatus::ForgetPending
        && next.sync_status == SyncStatus::NotConfigured
        && next.view == AppView::Wireless)
        .then_some(StorageCommand::ForgetWifiCredentials)
}

fn storage_command_for_transition(
    previous: &ReaderState,
    next: &ReaderState,
) -> Option<StorageCommand> {
    if next.view == AppView::FirmwareUpdate && previous.view != AppView::FirmwareUpdate {
        return Some(StorageCommand::ScanFirmwareFiles);
    }
    if next.view == AppView::FirmwareUpdate
        && previous.firmware_status != app_core::FirmwareUpdateStatus::Staging
        && next.firmware_status == app_core::FirmwareUpdateStatus::Staging
    {
        return Some(StorageCommand::StageFirmwareUpdate {
            index: next.selection,
        });
    }
    if next.view == AppView::FirmwareUpdate
        && previous.firmware_status == app_core::FirmwareUpdateStatus::Failed
        && next.firmware_status == app_core::FirmwareUpdateStatus::Scanning
    {
        return Some(StorageCommand::ScanFirmwareFiles);
    }
    let index = ReaderSource::from_book_id(next.book_id).sd_index()?;
    // Entering the overview loads the full chapter list into the section
    // buffer; the reading section reloads on exit.
    if next.view == AppView::Chapters && previous.view != AppView::Chapters {
        return Some(load_chapters_command(next, index));
    }
    if next.view != AppView::Reading {
        return None;
    }

    if previous.book_id != next.book_id {
        return Some(open_book_command(next, index));
    }

    if previous.view != AppView::Reading {
        if previous.view == AppView::Chapters {
            // The buffer held the TOC, so the section always reloads. A new
            // chapter selection resolves its page from the on-disk TOC; a
            // plain back-out just reloads the page we left.
            return if next.selection != previous.chapter {
                Some(jump_chapter_command(next, index))
            } else {
                Some(extend_section_command(next, index))
            };
        }
        // An unchanged book id no longer proves the store holds its
        // pages: boot restore and the scan default set the active book
        // without loading anything. Entering Reading always requests
        // the section; an already-loaded book answers from RAM without
        // an SD session.
        return Some(open_book_command(next, index));
    }

    if previous.page != next.page
        || previous.chapter != next.chapter
        || previous.pending_page != next.pending_page
    {
        return Some(extend_section_command(next, index));
    }

    None
}

fn open_book_command(state: &ReaderState, index: u16) -> StorageCommand {
    let request_id = next_reader_request_id();
    StorageCommand::OpenBook {
        request_id,
        book_id: state.book_id,
        index,
        chapter: state.chapter,
        target_pages: state.page.min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
        portrait: app_core::is_portrait(state.orientation),
    }
}

fn extend_section_command(state: &ReaderState, index: u16) -> StorageCommand {
    let request_id = next_reader_request_id();
    StorageCommand::ExtendSection {
        request_id,
        book_id: state.book_id,
        index,
        chapter: state.chapter,
        target_pages: state
            .pending_page
            .unwrap_or(state.page)
            .min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
        portrait: app_core::is_portrait(state.orientation),
    }
}

fn load_chapters_command(state: &ReaderState, index: u16) -> StorageCommand {
    StorageCommand::LoadChapters {
        request_id: next_reader_request_id(),
        book_id: state.book_id,
        index,
    }
}

fn jump_chapter_command(state: &ReaderState, index: u16) -> StorageCommand {
    StorageCommand::JumpChapter {
        request_id: next_reader_request_id(),
        book_id: state.book_id,
        index,
        chapter: state.selection,
        type_settings: state.type_settings(),
        portrait: app_core::is_portrait(state.orientation),
    }
}

fn next_reader_request_id() -> u32 {
    let next = LATEST_READER_REQUEST_ID
        .load(Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    LATEST_READER_REQUEST_ID.store(next, Ordering::Relaxed);
    next
}
