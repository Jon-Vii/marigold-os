use crate::{
    catalog, AppView, Button, DisplayCommand, DisplayEvent, DisplayOrientation, InputEvent,
    LibraryEvent, PowerEvent, RefreshPolicy, RenderKind, RenderRequest, DISPLAY_COMMANDS,
    DISPLAY_EVENTS, INPUT_EVENTS, LIBRARY_EVENTS, POWER_EVENTS,
};
use display::Rect;
use embassy_futures::select::{select3, Either3};
use hal_ext::nvm::AppStateRecord;

const SETTINGS_ITEMS: u8 = 3;
const MAX_SD_CHAPTERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReaderState {
    view: AppView,
    page: u32,
    selection: u8,
    chapter: u8,
    book_id: u32,
    orientation: DisplayOrientation,
    refresh_policy: RefreshPolicy,
    last_button: Option<Button>,
    aux_raw: u16,
    nav_raw: u16,
    page_raw: u16,
    battery_mv: u16,
    battery_percent: u8,
    library_count: u8,
    sd_page_count: u32,
    sd_chapter_count: u8,
    sd_chapter_pages: [u16; MAX_SD_CHAPTERS],
    read_request_pending: bool,
    dirty: Rect,
}

impl ReaderState {
    const fn boot() -> Self {
        Self {
            view: AppView::Home,
            page: 0,
            selection: 0,
            chapter: 0,
            book_id: 1,
            orientation: DisplayOrientation::LandscapeButtonsBottom,
            refresh_policy: RefreshPolicy::FullOnWake,
            last_button: None,
            aux_raw: 0,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 0,
            battery_percent: 100,
            library_count: 0,
            sd_page_count: 1,
            sd_chapter_count: 1,
            sd_chapter_pages: [0; MAX_SD_CHAPTERS],
            read_request_pending: false,
            dirty: Rect::FULL,
        }
    }

    fn apply(self, event: InputEvent) -> Self {
        let InputEvent::Sample {
            button,
            aux_raw,
            nav_raw,
            page_raw,
            battery_mv,
            battery_percent,
        } = event;
        let mut next = self;
        next.last_button = button;
        next.aux_raw = aux_raw;
        next.nav_raw = nav_raw;
        next.page_raw = page_raw;
        next.battery_mv = battery_mv;
        next.battery_percent = battery_percent;
        next.dirty = Rect::FULL;

        match (self.view, button) {
            (_, None) => {}
            (_, Some(Button::Power)) => {}
            (AppView::Home, Some(Button::Back)) => {
                if self.book_id >= 2 {
                    next.view = AppView::Reading;
                    next.selection = self.chapter;
                    next.read_request_pending = false;
                } else {
                    next.view = AppView::Library;
                    next.selection = 0;
                    next.read_request_pending = true;
                }
            }
            (AppView::Home, Some(Button::Confirm)) => {
                next.view = AppView::Library;
                next.selection = 0;
                next.read_request_pending = false;
            }
            (AppView::Home, Some(Button::Previous)) => {
                next.view = AppView::Sync;
                next.selection = 0;
                next.read_request_pending = false;
            }
            (AppView::Home, Some(Button::Next)) => {
                next.view = AppView::Settings;
                next.selection = 0;
                next.read_request_pending = false;
            }

            (AppView::Library, Some(Button::Next)) => {
                next.selection = wrap_next(self.selection, self.library_item_count());
            }
            (AppView::Library, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, self.library_item_count());
            }
            (AppView::Library, Some(Button::Confirm)) => {
                if self.selection < self.library_count {
                    next.book_id = self.selection as u32 + 2;
                    next.view = AppView::Reading;
                    next.chapter = 0;
                    next.selection = 0;
                    next.page = 0;
                    next.sd_page_count = 1;
                    next.sd_chapter_count = 1;
                    next.sd_chapter_pages = [0; MAX_SD_CHAPTERS];
                    next.read_request_pending = false;
                } else if let Some(book) = catalog::book_at(self.selection as usize) {
                    next.book_id = book.id.0;
                    next.view = AppView::Reading;
                    next.selection = 0;
                    next.read_request_pending = false;
                }
            }
            (AppView::Library, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 1;
                next.read_request_pending = false;
            }

            (AppView::Reading, Some(Button::Next)) => {
                if self.book_id >= 2 {
                    if self.page + 1 < self.sd_page_count {
                        next.page = self.page + 1;
                    } else if self.chapter + 1 < self.sd_chapter_count {
                        next.chapter = self.chapter + 1;
                        next.selection = next.chapter;
                        next.page = 0;
                    } else {
                        next.page = self.sd_page_count.saturating_sub(1);
                    }
                } else {
                    next.chapter = wrap_next(self.chapter, catalog::chapter_count());
                    next.selection = next.chapter;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Previous)) => {
                if self.book_id >= 2 {
                    if self.page > 0 {
                        next.page = self.page - 1;
                    } else if self.chapter > 0 {
                        next.chapter = self.chapter - 1;
                        next.selection = next.chapter;
                        next.page = 0;
                    }
                } else {
                    next.chapter = wrap_prev(self.chapter, catalog::chapter_count());
                    next.selection = next.chapter;
                    next.page = 0;
                }
            }
            (AppView::Reading, Some(Button::Confirm)) => {
                next.view = AppView::Chapters;
                next.selection = if self.book_id >= 2 {
                    self.sd_chapter_for_page(self.page)
                } else {
                    self.chapter
                };
            }
            (AppView::Reading, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }

            (AppView::Chapters, Some(Button::Next)) => {
                next.selection = wrap_next(self.selection, self.chapter_item_count());
            }
            (AppView::Chapters, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, self.chapter_item_count());
            }
            (AppView::Chapters, Some(Button::Confirm)) => {
                next.chapter = self.selection;
                next.page = 0;
                next.view = AppView::Reading;
            }
            (AppView::Chapters, Some(Button::Back)) => {
                next.view = AppView::Reading;
            }

            (AppView::Sync, Some(Button::Back | Button::Confirm)) => {
                next.view = AppView::Home;
                next.selection = 0;
            }
            (AppView::Sync, Some(Button::Previous | Button::Next)) => {}

            (AppView::Settings, Some(Button::Next)) => {
                next.selection = wrap_next(self.selection, SETTINGS_ITEMS);
            }
            (AppView::Settings, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, SETTINGS_ITEMS);
            }
            (AppView::Settings, Some(Button::Confirm)) => {
                next = apply_setting(next);
            }
            (AppView::Settings, Some(Button::Back)) => {
                next.view = AppView::Home;
                next.selection = 2;
            }
        }

        next
    }

    fn apply_library_event(mut self, event: LibraryEvent) -> Self {
        match event {
            LibraryEvent::Scanned { count } => {
                self.library_count = count;
                if self.view == AppView::Library {
                    if self.read_request_pending && count == 0 {
                        self.book_id = 1;
                        self.chapter = 0;
                        self.page = 0;
                        self.selection = 0;
                        self.view = AppView::Reading;
                        self.read_request_pending = false;
                    } else if count == 0 {
                        self.selection = 0;
                    } else if self.selection >= count {
                        self.selection = count - 1;
                    }
                    if count > 0 {
                        self.read_request_pending = false;
                    }
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::Loaded {
                book_id,
                pages,
                chapters,
            } => {
                if self.book_id == book_id {
                    self.sd_page_count = pages.max(1);
                    self.sd_chapter_count = chapters.max(1);
                    self.sd_chapter_pages = [0; MAX_SD_CHAPTERS];
                    self.page = self.page.min(self.sd_page_count.saturating_sub(1));
                    self.dirty = Rect::FULL;
                }
            }
            LibraryEvent::ChapterPage {
                book_id,
                chapter,
                page,
            } => {
                if self.book_id == book_id {
                    if let Some(slot) = self.sd_chapter_pages.get_mut(chapter as usize) {
                        *slot = page.min(u16::MAX as u32) as u16;
                    }
                }
            }
            LibraryEvent::Restored {
                book_id,
                chapter,
                page,
                reading_orientation,
                refresh_policy,
            } => {
                self.book_id = book_id;
                self.chapter = chapter;
                self.selection = chapter;
                self.page = page;
                self.read_request_pending = false;
                if let Some(orientation) = display_orientation_from_u8(reading_orientation) {
                    self.orientation = orientation;
                }
                if let Some(policy) = refresh_policy_from_u8(refresh_policy) {
                    self.refresh_policy = policy;
                }
                self.dirty = Rect::FULL;
            }
        }
        if self.view == AppView::Library {
            self.dirty = Rect::FULL;
        }
        self
    }

    fn library_item_count(self) -> u8 {
        self.library_count.max(catalog::book_count()).max(1)
    }

    fn chapter_item_count(self) -> u8 {
        if self.book_id >= 2 {
            self.sd_chapter_count.max(1)
        } else {
            catalog::chapter_count()
        }
    }

    fn sd_chapter_for_page(self, page: u32) -> u8 {
        let mut selected = 0u8;
        for index in 0..self.sd_chapter_count.min(MAX_SD_CHAPTERS as u8) {
            if u32::from(self.sd_chapter_pages[index as usize]) <= page {
                selected = index;
            } else {
                break;
            }
        }
        selected
    }

    fn persisted(self) -> AppStateRecord {
        AppStateRecord {
            book_id: self.book_id,
            chapter: self.chapter as u16,
            screen: self.page,
            shell_orientation: DisplayOrientation::PortraitButtonsLeft as u8,
            reading_orientation: self.orientation as u8,
            refresh_policy: self.refresh_policy as u8,
            source_hash: 0,
            source_size: 0,
        }
    }
}

fn display_orientation_from_u8(value: u8) -> Option<DisplayOrientation> {
    match value {
        0 => Some(DisplayOrientation::LandscapeButtonsBottom),
        1 => Some(DisplayOrientation::LandscapeButtonsTop),
        2 => Some(DisplayOrientation::PortraitButtonsLeft),
        3 => Some(DisplayOrientation::PortraitButtonsRight),
        _ => None,
    }
}

fn refresh_policy_from_u8(value: u8) -> Option<RefreshPolicy> {
    match value {
        0 => Some(RefreshPolicy::FastOnly),
        1 => Some(RefreshPolicy::FullOnWake),
        2 => Some(RefreshPolicy::FullEveryTen),
        _ => None,
    }
}

fn wrap_next(value: u8, len: u8) -> u8 {
    if value + 1 >= len {
        0
    } else {
        value + 1
    }
}

fn wrap_prev(value: u8, len: u8) -> u8 {
    if value == 0 {
        len - 1
    } else {
        value - 1
    }
}

fn apply_setting(mut state: ReaderState) -> ReaderState {
    match state.selection {
        0 => {
            state.orientation = match state.orientation {
                DisplayOrientation::LandscapeButtonsBottom => {
                    DisplayOrientation::LandscapeButtonsTop
                }
                DisplayOrientation::LandscapeButtonsTop => DisplayOrientation::PortraitButtonsLeft,
                DisplayOrientation::PortraitButtonsLeft => DisplayOrientation::PortraitButtonsRight,
                DisplayOrientation::PortraitButtonsRight => {
                    DisplayOrientation::LandscapeButtonsBottom
                }
            };
        }
        1 => {
            state.refresh_policy = match state.refresh_policy {
                RefreshPolicy::FastOnly => RefreshPolicy::FullOnWake,
                RefreshPolicy::FullOnWake => RefreshPolicy::FullEveryTen,
                RefreshPolicy::FullEveryTen => RefreshPolicy::FastOnly,
            };
        }
        _ => {
            state.view = AppView::Home;
            state.selection = 2;
        }
    }
    state
}

#[embassy_executor::task]
pub async fn run() {
    esp_println::println!("app: started");
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
                        sleeping = false;
                        state.view = AppView::Home;
                        state.dirty = Rect::FULL;
                        let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                        send_render(RenderKind::Page, state).await;
                        rendering = true;
                        render_pending = false;
                    } else {
                        sleeping = true;
                        let _ = POWER_EVENTS.try_send(PowerEvent::SleepNow);
                    }
                    continue;
                }

                if sleeping {
                    continue;
                }

                let _ = POWER_EVENTS.try_send(PowerEvent::Activity);
                state = state.apply(event);
                let _pending_persist = state.persisted();
                if rendering {
                    render_pending = true;
                } else {
                    send_render(RenderKind::Page, state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either3::Second(DisplayEvent::Settled) => {
                rendering = false;
                if render_pending {
                    send_render(RenderKind::Page, state).await;
                    rendering = true;
                    render_pending = false;
                }
            }
            Either3::Third(event) => {
                state = state.apply_library_event(event);
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
        .send(DisplayCommand::Render(RenderRequest {
            kind,
            view: state.view,
            page: state.page,
            chapter: state.chapter,
            selection: state.selection,
            book_id: state.book_id,
            orientation: state.orientation,
            refresh_policy: state.refresh_policy,
            last_button: state.last_button,
            aux_raw: state.aux_raw,
            nav_raw: state.nav_raw,
            page_raw: state.page_raw,
            battery_mv: state.battery_mv,
            battery_percent: state.battery_percent,
            dirty: state.dirty,
        }))
        .await;
}
