use crate::{
    catalog, Button, DisplayCommand, DisplayEvent, InputEvent, PowerEvent, RenderKind,
    DISPLAY_COMMANDS, DISPLAY_EVENTS, INPUT_EVENTS, LIBRARY_EVENTS, POWER_EVENTS,
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
                        let _ = POWER_EVENTS.try_send(PowerEvent::SleepNow);
                    }
                    continue;
                }

                if sleeping {
                    continue;
                }

                let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                state = state.apply_input(ctx, event);
                let _pending_persist = state.persisted();
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
