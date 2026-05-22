#![no_std]
#![forbid(unsafe_code)]

use display::Rect;

pub const SETTINGS_ITEMS: u8 = 3;
pub const MAX_SD_CHAPTERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Power,
    Back,
    Confirm,
    Previous,
    Next,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Sample {
        button: Option<Button>,
        aux_raw: u16,
        nav_raw: u16,
        page_raw: u16,
        battery_mv: u16,
        battery_percent: u8,
    },
}

impl InputEvent {
    pub const fn button(button: Button) -> Self {
        Self::Sample {
            button: Some(button),
            aux_raw: 2000,
            nav_raw: 0,
            page_raw: 0,
            battery_mv: 4000,
            battery_percent: 77,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderKind {
    Boot,
    Page,
    Battery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOrientation {
    LandscapeButtonsBottom,
    LandscapeButtonsTop,
    PortraitButtonsLeft,
    PortraitButtonsRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppView {
    Home,
    Library,
    Reading,
    Chapters,
    Sync,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPolicy {
    FastOnly,
    FullOnWake,
    FullEveryTen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderRequest {
    pub kind: RenderKind,
    pub view: AppView,
    pub page: u32,
    pub chapter: u8,
    pub selection: u8,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub refresh_policy: RefreshPolicy,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u8,
    pub dirty: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayCommand {
    Render(RenderRequest),
    Sleep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCommand {
    RefreshCatalog,
    OpenBook {
        book_id: u32,
        index: u8,
        chapter: u8,
        target_pages: u16,
    },
    ExtendSection {
        book_id: u32,
        index: u8,
        chapter: u8,
        target_pages: u16,
    },
    StoreProgress(PersistedAppState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayEvent {
    Settled,
    Asleep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryEvent {
    Scanned {
        count: u8,
    },
    Loaded {
        book_id: u32,
        pages: u32,
        chapters: u8,
    },
    ChapterPage {
        book_id: u32,
        chapter: u8,
        page: u32,
    },
    Restored {
        book_id: u32,
        chapter: u8,
        page: u32,
        reading_orientation: u8,
        refresh_policy: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    Activity,
    DisplaySettled,
    DisplayAsleep,
    SleepNow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistedAppState {
    pub book_id: u32,
    pub chapter: u16,
    pub screen: u32,
    pub shell_orientation: u8,
    pub reading_orientation: u8,
    pub refresh_policy: u8,
    pub source_hash: u32,
    pub source_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReducerContext {
    pub builtin_book_count: u8,
    pub builtin_chapter_count: u8,
}

impl ReducerContext {
    pub const fn new(builtin_book_count: u8, builtin_chapter_count: u8) -> Self {
        Self {
            builtin_book_count,
            builtin_chapter_count,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderState {
    pub view: AppView,
    pub page: u32,
    pub selection: u8,
    pub chapter: u8,
    pub book_id: u32,
    pub orientation: DisplayOrientation,
    pub refresh_policy: RefreshPolicy,
    pub last_button: Option<Button>,
    pub aux_raw: u16,
    pub nav_raw: u16,
    pub page_raw: u16,
    pub battery_mv: u16,
    pub battery_percent: u8,
    pub library_count: u8,
    pub sd_page_count: u32,
    pub sd_chapter_count: u8,
    pub sd_chapter_pages: [u16; MAX_SD_CHAPTERS],
    pub read_request_pending: bool,
    pub dirty: Rect,
}

impl ReaderState {
    pub const fn boot() -> Self {
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

    pub fn apply_input(self, ctx: ReducerContext, event: InputEvent) -> Self {
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
                next.view = AppView::Library;
                next.selection = 0;
                next.read_request_pending = false;
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
                next.selection = wrap_next(self.selection, self.library_item_count(ctx));
            }
            (AppView::Library, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, self.library_item_count(ctx));
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
                } else if self.selection < self.library_item_count(ctx) {
                    next.book_id = 1;
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
                    next.chapter = wrap_next(self.chapter, ctx.builtin_chapter_count.max(1));
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
                    next.chapter = wrap_prev(self.chapter, ctx.builtin_chapter_count.max(1));
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
                next.selection = wrap_next(self.selection, self.chapter_item_count(ctx));
            }
            (AppView::Chapters, Some(Button::Previous)) => {
                next.selection = wrap_prev(self.selection, self.chapter_item_count(ctx));
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

    pub fn apply_library_event(mut self, ctx: ReducerContext, event: LibraryEvent) -> Self {
        match event {
            LibraryEvent::Scanned { count } => {
                self.library_count = count;
                if self.view == AppView::Library {
                    if count == 0 {
                        self.selection = 0;
                    } else if self.selection >= count {
                        self.selection = count - 1;
                    }
                    self.dirty = Rect::FULL;
                    if self.read_request_pending {
                        self.read_request_pending = false;
                    }
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
                    self.dirty = Rect::FULL;
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
                self.page = page;
                if self.read_request_pending {
                    self.view = AppView::Reading;
                    self.selection = chapter;
                } else if self.view == AppView::Library {
                    let restored_index = book_id.saturating_sub(2).min(u8::MAX as u32) as u8;
                    self.selection = restored_index.min(self.library_count.saturating_sub(1));
                } else {
                    self.selection = chapter;
                }
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
            self.selection = self
                .selection
                .min(self.library_item_count(ctx).saturating_sub(1));
            self.dirty = Rect::FULL;
        }
        self
    }

    pub fn render_request(self, kind: RenderKind) -> RenderRequest {
        RenderRequest {
            kind,
            view: self.view,
            page: self.page,
            chapter: self.chapter,
            selection: self.selection,
            book_id: self.book_id,
            orientation: self.orientation,
            refresh_policy: self.refresh_policy,
            last_button: self.last_button,
            aux_raw: self.aux_raw,
            nav_raw: self.nav_raw,
            page_raw: self.page_raw,
            battery_mv: self.battery_mv,
            battery_percent: self.battery_percent,
            library_count: self.library_count,
            dirty: self.dirty,
        }
    }

    pub fn persisted(self) -> PersistedAppState {
        PersistedAppState {
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

    pub fn library_item_count(self, ctx: ReducerContext) -> u8 {
        self.library_count.max(ctx.builtin_book_count).max(1)
    }

    pub fn chapter_item_count(self, ctx: ReducerContext) -> u8 {
        if self.book_id >= 2 {
            self.sd_chapter_count.max(1)
        } else {
            ctx.builtin_chapter_count.max(1)
        }
    }

    pub fn sd_chapter_for_page(self, page: u32) -> u8 {
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
}

pub fn display_orientation_from_u8(value: u8) -> Option<DisplayOrientation> {
    match value {
        0 => Some(DisplayOrientation::LandscapeButtonsBottom),
        1 => Some(DisplayOrientation::LandscapeButtonsTop),
        2 => Some(DisplayOrientation::PortraitButtonsLeft),
        3 => Some(DisplayOrientation::PortraitButtonsRight),
        _ => None,
    }
}

pub fn refresh_policy_from_u8(value: u8) -> Option<RefreshPolicy> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const CTX: ReducerContext = ReducerContext::new(1, 3);

    fn press(state: ReaderState, button: Button) -> ReaderState {
        state.apply_input(CTX, InputEvent::button(button))
    }

    #[test]
    fn home_navigation_opens_primary_views() {
        assert_eq!(
            press(ReaderState::boot(), Button::Confirm).view,
            AppView::Library
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Back).view,
            AppView::Library
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Previous).view,
            AppView::Sync
        );
        assert_eq!(
            press(ReaderState::boot(), Button::Next).view,
            AppView::Settings
        );
    }

    #[test]
    fn library_selection_opens_sd_book() {
        let state = press(ReaderState::boot(), Button::Back)
            .apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.book_id, 3);
    }

    #[test]
    fn reading_next_previous_bounds_sd_pages() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = 2;
        state.sd_page_count = 2;
        assert_eq!(press(state, Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Next).page, 1);
        assert_eq!(press(press(state, Button::Next), Button::Previous).page, 0);
    }

    #[test]
    fn chapter_selection_changes_reading_chapter() {
        let mut state = ReaderState::boot();
        state.view = AppView::Reading;
        state.book_id = 1;
        let state = press(state, Button::Confirm);
        let state = press(press(state, Button::Next), Button::Confirm);
        assert_eq!(state.view, AppView::Reading);
        assert_eq!(state.chapter, 1);
    }

    #[test]
    fn catalog_scan_does_not_auto_open_from_files() {
        let state = press(ReaderState::boot(), Button::Back);
        assert_eq!(state.view, AppView::Library);
        assert!(!state.read_request_pending);

        let state = state.apply_library_event(CTX, LibraryEvent::Scanned { count: 2 });
        assert_eq!(state.view, AppView::Library);
        assert_eq!(state.library_count, 2);
        assert!(!state.read_request_pending);
    }

    #[test]
    fn settings_cycle_orientation_and_refresh_policy() {
        let state = press(ReaderState::boot(), Button::Next);
        let state = press(state, Button::Confirm);
        assert_eq!(state.orientation, DisplayOrientation::LandscapeButtonsTop);
        let state = press(state, Button::Next);
        let state = press(state, Button::Confirm);
        assert_eq!(state.refresh_policy, RefreshPolicy::FullEveryTen);
    }

    #[test]
    fn library_restore_updates_progress_and_preferences() {
        let state = ReaderState::boot().apply_library_event(
            CTX,
            LibraryEvent::Restored {
                book_id: 2,
                chapter: 4,
                page: 12,
                reading_orientation: DisplayOrientation::PortraitButtonsRight as u8,
                refresh_policy: RefreshPolicy::FastOnly as u8,
            },
        );
        assert_eq!(state.book_id, 2);
        assert_eq!(state.chapter, 4);
        assert_eq!(state.page, 12);
        assert_eq!(state.orientation, DisplayOrientation::PortraitButtonsRight);
        assert_eq!(state.refresh_policy, RefreshPolicy::FastOnly);
    }
}
