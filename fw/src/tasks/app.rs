use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, RenderKind,
    StorageCommand, DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS, LIBRARY_EVENTS, POWER_EVENTS,
    STORAGE_COMMANDS,
};
use app_core::{AppView, ReaderState, ReducerContext};
use display::Rect;
use embassy_futures::select::{select3, Either3};

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
                        let _ = DISPLAY_COMMANDS.send(DisplayCommand::Sleep).await;
                    }
                    continue;
                }

                if sleeping {
                    continue;
                }

                let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                let previous = state;
                state = state.apply_input(ctx, event);
                let _pending_persist = state.persisted();
                if let Some(command) = storage_command_for_transition(previous, state) {
                    pending_storage = Some(command);
                }
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
                        STORAGE_COMMANDS
                            .send(StorageCommand::LoadCatalogCache)
                            .await;
                    }
                    if let Some(command) = pending_storage.take() {
                        STORAGE_COMMANDS.send(command).await;
                    }
                    if render_pending {
                        send_render(RenderKind::Page, state).await;
                        rendering = true;
                        render_pending = false;
                    }
                }
                DisplayEvent::Asleep => {
                    esp_println::println!("app: display asleep");
                    rendering = false;
                    render_pending = false;
                }
            },
            Either3::Third(event) => {
                state = state.apply_library_event(ctx, event);
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

async fn send_render(kind: RenderKind, state: ReaderState) {
    DISPLAY_COMMANDS
        .send(DisplayCommand::Render(state.render_request(kind)))
        .await;
}

fn reducer_context() -> ReducerContext {
    ReducerContext::new(catalog::book_count(), catalog::chapter_count())
}

fn storage_command_for_transition(
    previous: ReaderState,
    next: ReaderState,
) -> Option<StorageCommand> {
    if next.view != AppView::Reading || next.book_id < 2 {
        return None;
    }

    let index = next.book_id.saturating_sub(2).min(u8::MAX as u32) as u8;
    if previous.book_id != next.book_id
        || previous.chapter != next.chapter
        || previous.view != AppView::Reading
    {
        return Some(StorageCommand::OpenBook {
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: 5,
        });
    }

    if next.page.saturating_add(2) >= next.sd_page_count {
        return Some(StorageCommand::ExtendSection {
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: next.page.saturating_add(5).min(u16::MAX as u32) as u16,
        });
    }

    if previous.page != next.page {
        return Some(StorageCommand::StoreProgress(next.persisted()));
    }

    None
}
