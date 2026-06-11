use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, ReaderSource,
    RenderKind, StorageCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS,
    LATEST_READER_REQUEST_ID, LIBRARY_EVENTS, POWER_EVENTS, STORAGE_COMMANDS,
};
use app_core::{AppView, ReaderState, ReducerContext};
use core::sync::atomic::Ordering;
use display::Rect;
use embassy_futures::select::{select3, Either3};
use embassy_time::{Duration, Instant};

const POST_OPEN_CONFIRM_BLOCK_MS: u64 = 700;

#[embassy_executor::task]
pub async fn run() {
    esp_println::println!("app: started");
    let ctx = reducer_context();
    let mut state = ReaderState::boot();
    let mut rendering = true;
    let mut render_pending = false;
    let mut sleeping = false;
    let mut catalog_refresh_requested = false;
    let mut pending_storage: Option<StorageCommand> = None;
    // Type settings changed while away from Reading: the loaded section is
    // paginated under the old layout, so the next entry into Reading must
    // send an extend even though page and chapter are unchanged.
    let mut reader_relayout_pending = false;
    let mut opening_book: Option<u32> = None;
    let mut suppress_input_until_open_settled = false;
    let mut block_confirm_until: Option<Instant> = None;
    send_render(RenderKind::Boot, state).await;

    loop {
        match select3(
            INPUT_EVENTS.receive(),
            DISPLAY_EVENTS.receive(),
            LIBRARY_EVENTS.receive(),
        )
        .await
        {
            Either3::First(event) => {
                if matches!(
                    event,
                    InputEvent::Sample {
                        button: Some(Button::Power),
                        ..
                    }
                ) {
                    if sleeping {
                        esp_println::println!("app: wake");
                        sleeping = false;
                        state.view = AppView::Home;
                        state.dirty = Rect::FULL;
                        send_render(RenderKind::Page, state).await;
                        rendering = true;
                        render_pending = false;
                    } else {
                        esp_println::println!("app: sleep");
                        sleeping = true;
                        // Drop any queued repaint: a stale render arriving
                        // after the sleep command would wake the panel.
                        render_pending = false;
                        let _ = DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;
                    }
                    continue;
                }

                if sleeping {
                    continue;
                }

                if state.view == AppView::Reading
                    && should_block_post_open_confirm(event, &mut block_confirm_until)
                {
                    esp_println::println!("app: confirm ignored after book open");
                    continue;
                }

                if opening_book.is_some() || suppress_input_until_open_settled {
                    esp_println::println!("app: input ignored while book open pending");
                    continue;
                }

                let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                let previous = state;
                let previous_persisted = state.persisted();
                state = state.apply_input(ctx, event);
                let next_persisted = state.persisted();
                if previous.type_settings() != state.type_settings() {
                    reader_relayout_pending = true;
                }
                let mut storage_command = storage_command_for_transition(previous, state);
                if storage_command.is_none()
                    && reader_relayout_pending
                    && state.view == AppView::Reading
                {
                    if let Some(index) = ReaderSource::from_book_id(state.book_id).sd_index() {
                        storage_command = Some(extend_section_command(state, index));
                    }
                }
                if storage_command.is_some() {
                    // Open/extend commands carry the current type settings,
                    // so any dispatched command syncs the reader store.
                    reader_relayout_pending = false;
                }
                if let Some(command) = storage_command {
                    if should_send_storage_immediately(command) {
                        log_storage_command("send", command);
                        if let Some(book_id) = open_book_id(command) {
                            opening_book = Some(book_id);
                            suppress_input_until_open_settled = true;
                        }
                        if STORAGE_COMMANDS.try_send(command).is_err() {
                            log_storage_command("queue", command);
                            pending_storage = Some(command);
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
                if previous_persisted != next_persisted {
                    let command = StorageCommand::StoreProgress(next_persisted);
                    if STORAGE_COMMANDS.try_send(command).is_err() && pending_storage.is_none() {
                        pending_storage = Some(command);
                    }
                }
                // We used to suppress the render when an open was inflight
                // and wait for the Loaded event. That's fine when the cache
                // hits and the open returns in milliseconds, but on a cache
                // miss the rebuild can take a minute and the UI looks frozen
                // on the previous screen. Let the render through immediately:
                // the Reading view draws "OPENING EPUB" while sd_library's
                // loaded_index doesn't match the requested book.
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either3::Second(event) => match event {
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
                        if let Some(book_id) = open_book_id(command) {
                            opening_book = Some(book_id);
                            suppress_input_until_open_settled = true;
                        }
                        STORAGE_COMMANDS.send(command).await;
                    }
                    if render_pending && !sleeping {
                        send_render(RenderKind::Page, state).await;
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
                DisplayEvent::Library(event) => {
                    if let Some(book_id) = loaded_book_id(event) {
                        if opening_book == Some(book_id) {
                            opening_book = None;
                        }
                    }
                    let should_render = library_event_affects_view(state, event);
                    state = state.apply_library_event(ctx, event);
                    if !should_render || sleeping {
                        continue;
                    }
                    if rendering {
                        render_pending = true;
                    } else {
                        send_render(RenderKind::Page, state).await;
                        rendering = true;
                        render_pending = false;
                    }
                }
            },
            Either3::Third(event) => {
                if let Some(book_id) = loaded_book_id(event) {
                    if opening_book == Some(book_id) {
                        opening_book = None;
                    }
                }
                let should_render = library_event_affects_view(state, event);
                state = state.apply_library_event(ctx, event);
                if !should_render || sleeping {
                    continue;
                }
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
        }
    }
}

fn library_event_affects_view(state: ReaderState, event: crate::LibraryEvent) -> bool {
    match event {
        crate::LibraryEvent::Scanned { count } => {
            state.view == AppView::Library && state.library_count != count
        }
        crate::LibraryEvent::Loaded {
            book_id,
            pages: _,
            chapters: _,
            chapter_pages: _,
        } => state.book_id == book_id,
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
        crate::LibraryEvent::Restored { .. } => true,
    }
}

fn should_send_storage_immediately(command: StorageCommand) -> bool {
    matches!(
        command,
        StorageCommand::OpenBook { .. } | StorageCommand::ExtendSection { .. }
    )
}

fn open_book_id(command: StorageCommand) -> Option<u32> {
    match command {
        StorageCommand::OpenBook { book_id, .. } => Some(book_id),
        _ => None,
    }
}

fn loaded_book_id(event: crate::LibraryEvent) -> Option<u32> {
    match event {
        crate::LibraryEvent::Loaded { book_id, .. } => Some(book_id),
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

async fn send_render(kind: RenderKind, state: ReaderState) {
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
    }
}

fn reducer_context() -> ReducerContext {
    ReducerContext::new(catalog::book_count(), catalog::chapter_count())
}

fn storage_command_for_transition(
    previous: ReaderState,
    next: ReaderState,
) -> Option<StorageCommand> {
    let Some(index) = ReaderSource::from_book_id(next.book_id).sd_index() else {
        return None;
    };
    if next.view != AppView::Reading {
        return None;
    }

    if previous.book_id != next.book_id {
        return Some(open_book_command(next, index));
    }

    if previous.view != AppView::Reading {
        if previous.view == AppView::Chapters {
            // Chapters is only reachable from Reading, so the book is
            // loaded; backing out unchanged needs no request.
            return if previous.page != next.page || previous.chapter != next.chapter {
                Some(extend_section_command(next, index))
            } else {
                None
            };
        }
        // An unchanged book id no longer proves the store holds its
        // pages: boot restore and the scan default set the active book
        // without loading anything. Entering Reading always requests
        // the section; an already-loaded book answers from RAM without
        // an SD session.
        return Some(open_book_command(next, index));
    }

    if previous.page != next.page || previous.chapter != next.chapter {
        return Some(extend_section_command(next, index));
    }

    None
}

fn open_book_command(state: ReaderState, index: u8) -> StorageCommand {
    let request_id = next_reader_request_id();
    StorageCommand::OpenBook {
        request_id,
        book_id: state.book_id,
        index,
        chapter: state.chapter,
        target_pages: state.page.min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
    }
}

fn extend_section_command(state: ReaderState, index: u8) -> StorageCommand {
    let request_id = next_reader_request_id();
    StorageCommand::ExtendSection {
        request_id,
        book_id: state.book_id,
        index,
        chapter: state.chapter,
        target_pages: state.page.min(u16::MAX as u32) as u16,
        type_settings: state.type_settings(),
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
