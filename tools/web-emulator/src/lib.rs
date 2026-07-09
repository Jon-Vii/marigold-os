//! Browser build of the X4 emulator: the firmware's reducer, renderer, and
//! reading surface compiled to wasm32-unknown-unknown behind a small raw
//! C ABI. The page's JS feeds key presses and a monotonic clock in; it reads
//! frames back as RGBA straight out of wasm memory. No wasm-bindgen — the
//! surface is a handful of scalars and two buffer pointers.

mod books;

use app_core::{
    AppView, Button, DisplayOrientation, InputEvent, LibraryEvent, ReaderSource, RefreshPlanner,
    RenderKind, StorageCommand, SyncEvent, SyncStatus, WifiSsid, MAX_SD_CHAPTERS,
};
use books::{BookStore, SHELF};
use display::epd::RefreshMode;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontStyle, TypeSettings};
use display::{HEIGHT, WIDTH};
use ui::app_render::{render_request as render_shared, render_sleep as render_shared_sleep, UiRenderModel};
use ui::reading::ReadingBlocks;
use ui::{UiBook, UiLibraryStatus, UiTocItem};

const PAPER: [u8; 3] = [238, 236, 226];
const INK: [u8; 3] = [22, 22, 20];

/// Simulated card latencies, in ms of the caller's clock.
const OPEN_BOOK_MS: f64 = 650.0;
const REOPEN_BOOK_MS: f64 = 180.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadStatus {
    Empty,
    Loading,
    Ready,
}

enum Op {
    FinishOpen { book_index: u16 },
    Sync(SyncEvent),
}

struct WebEmulator {
    state: app_core::ReaderState,
    ctx: app_core::ReducerContext,
    planner: RefreshPlanner,
    fb: Framebuffer,
    rgba: Vec<u8>,
    sleeping: bool,
    store: Option<BookStore>,
    store_book: Option<u16>,
    load_status: LoadStatus,
    ops: Vec<(f64, Op)>,
    frame_seq: u32,
    last_refresh: u32,
    snapshot: [u32; 9],
}

impl WebEmulator {
    fn boot() -> Self {
        let mut emu = Self {
            state: app_core::ReaderState::boot(),
            ctx: app_core::ReducerContext::new(1, 4),
            planner: RefreshPlanner::new(),
            fb: Framebuffer::new(),
            rgba: vec![0; WIDTH * HEIGHT * 4],
            sleeping: false,
            store: None,
            store_book: None,
            load_status: LoadStatus::Empty,
            ops: Vec::new(),
            frame_seq: 0,
            last_refresh: 0,
            snapshot: [0; 9],
        };
        emu.state = emu.state.apply_library_event(
            emu.ctx,
            LibraryEvent::Scanned {
                count: SHELF.len() as u16,
            },
        );
        emu.restore_active_book(ReaderSource::sd(0).book_id(), 0, 0);
        // The firmware's boot probe of /XTEINK/WIFI.BIN, pretended: a saved
        // network so the Wireless screen opens on the connect/forget offer.
        // Forgetting it exposes the portal flow, which "saves" it back.
        emu.state = emu
            .state
            .apply_sync_event(SyncEvent::NetworkSaved(home_network()));
        emu.hydrate_book_metadata(0);
        emu.apply_loaded_metadata(0);
        emu.render(RenderKind::Boot);
        emu
    }

    fn input(&mut self, button: Button, now: f64) {
        if button == Button::Power {
            if self.sleeping {
                self.sleeping = false;
                self.state.view = AppView::Home;
                self.render(RenderKind::Page);
            } else {
                self.sleeping = true;
                self.render_sleep();
            }
            return;
        }
        if self.sleeping {
            return;
        }
        let previous = self.state;
        self.state = self.state.apply_input(
            self.ctx,
            InputEvent::Sample {
                button: Some(button),
                aux_raw: 2000,
                nav_raw: 0,
                page_raw: 0,
                battery_mv: 4012,
                battery_percent: 87,
            },
        );

        if let Some(command) = storage_command_for_transition(previous, self.state) {
            match command {
                StorageCommand::OpenBook { book_id, .. } => {
                    let book_index = ReaderSource::from_book_id(book_id).sd_index().unwrap_or(0);
                    let warm = self.store_ready_for(book_index);
                    if !warm {
                        self.load_status = LoadStatus::Loading;
                    }
                    let delay = if warm { REOPEN_BOOK_MS } else { OPEN_BOOK_MS };
                    self.ops.push((now + delay, Op::FinishOpen { book_index }));
                }
                StorageCommand::ExtendSection { .. } | StorageCommand::StoreProgress(_) => {
                    // The whole fake book is resident; progress persistence
                    // is the page's job via the snapshot buffer.
                }
                _ => {}
            }
        }

        if self.state.sync_status == SyncStatus::Starting
            && previous.sync_status != SyncStatus::Starting
        {
            if self.state.wifi_network_saved() {
                // The real session joins the network and then serves the book
                // upload page until the user finishes; there is no separate
                // progress-exchange step to pretend at.
                self.ops.push((now + 500.0, Op::Sync(SyncEvent::Connecting)));
                self.ops
                    .push((now + 1600.0, Op::Sync(SyncEvent::Connected([192, 168, 1, 27]))));
                self.ops
                    .push((now + 2600.0, Op::Sync(SyncEvent::Serving([192, 168, 1, 27]))));
            } else {
                // No saved network: the onboarding hotspot comes up and a
                // pretend phone submits credentials a few seconds later.
                self.ops.push((now + 900.0, Op::Sync(SyncEvent::PortalUp)));
                self.ops.push((
                    now + 7000.0,
                    Op::Sync(SyncEvent::CredentialsSaved(home_network())),
                ));
            }
        }
        if self.state.view != AppView::Wireless {
            self.ops.retain(|(_, op)| !matches!(op, Op::Sync(_)));
        }

        self.render(RenderKind::Page);
    }

    fn tick(&mut self, now: f64) {
        let mut due: Vec<Op> = Vec::new();
        self.ops.retain_mut(|(deadline, op)| {
            if *deadline <= now {
                due.push(match op {
                    Op::FinishOpen { book_index } => Op::FinishOpen {
                        book_index: *book_index,
                    },
                    Op::Sync(event) => Op::Sync(*event),
                });
                false
            } else {
                true
            }
        });
        for op in due {
            match op {
                Op::FinishOpen { book_index } => self.finish_open(book_index),
                Op::Sync(event) => {
                    if self.state.view == AppView::Wireless {
                        self.state = self.state.apply_sync_event(event);
                        self.render(RenderKind::Page);
                    }
                }
            }
        }
    }

    fn store_ready_for(&self, book_index: u16) -> bool {
        self.store_book == Some(book_index)
            && self
                .store
                .as_ref()
                .is_some_and(|store| store.type_settings() == self.state.type_settings())
            && self.load_status == LoadStatus::Ready
    }

    fn finish_open(&mut self, book_index: u16) {
        self.hydrate_book_metadata(book_index);
        self.apply_loaded_metadata(book_index);
        self.render(RenderKind::Page);
    }

    fn hydrate_book_metadata(&mut self, book_index: u16) {
        if self.store_ready_for(book_index) {
            return;
        }
        let source = &SHELF[book_index as usize % SHELF.len()];
        self.store = Some(BookStore::build(source, self.state.type_settings()));
        self.store_book = Some(book_index);
        self.load_status = LoadStatus::Ready;
    }

    fn apply_loaded_metadata(&mut self, book_index: u16) {
        let store = self.store.as_ref().unwrap();
        let mut chapter_pages = [0u16; MAX_SD_CHAPTERS];
        for (slot, chapter) in chapter_pages.iter_mut().zip(store.chapters.iter()) {
            *slot = chapter.start_page;
        }
        let event = LibraryEvent::Loaded {
            book_id: ReaderSource::sd(book_index).book_id(),
            pages: store.page_count(),
            chapters: store.chapters.len().max(1) as u8,
            current_chapter: store.chapter_for_page(self.state.page),
            chapter_pages,
        };
        self.state = self.state.apply_library_event(self.ctx, event);
    }

    fn restore_active_book(&mut self, book_id: u32, chapter: u32, page: u32) {
        self.state = self.state.apply_library_event(
            self.ctx,
            LibraryEvent::Restored {
                book_id,
                chapter: chapter.min(u8::MAX as u32) as u8,
                page,
                page_count: 0,
                reading_orientation: self.state.orientation as u8,
                refresh_policy: self.state.refresh_policy as u8,
                font_size: self.state.font_size as u8,
                line_spacing: self.state.line_spacing as u8,
                font_weight: self.state.font_weight as u8,
                font_family: self.state.font_family as u8,
            },
        );
    }

    fn restore(&mut self, snapshot: [u32; 9]) {
        self.state = self.state.apply_library_event(
            self.ctx,
            LibraryEvent::Restored {
                book_id: snapshot[0],
                chapter: snapshot[1].min(u8::MAX as u32) as u8,
                page: snapshot[2],
                page_count: 0,
                reading_orientation: snapshot[3] as u8,
                refresh_policy: snapshot[4] as u8,
                font_size: snapshot[5] as u8,
                line_spacing: snapshot[6] as u8,
                font_weight: snapshot[7] as u8,
                font_family: snapshot[8] as u8,
            },
        );
        let book_index = ReaderSource::from_book_id(snapshot[0]).sd_index().unwrap_or(0);
        self.hydrate_book_metadata(book_index);
        self.apply_loaded_metadata(book_index);
        self.render(RenderKind::Page);
    }

    fn refresh_snapshot(&mut self) {
        let persisted = self.state.persisted();
        self.snapshot = [
            persisted.book_id,
            u32::from(persisted.chapter),
            persisted.screen,
            u32::from(persisted.reading_orientation),
            u32::from(persisted.refresh_policy),
            u32::from(persisted.font_size),
            u32::from(persisted.line_spacing),
            u32::from(persisted.font_weight),
            u32::from(persisted.font_family),
        ];
    }

    fn render(&mut self, kind: RenderKind) {
        let request = self.state.render_request(kind);
        let sd_reading = request.view == AppView::Reading
            && ReaderSource::from_book_id(request.book_id).is_sd();
        if sd_reading {
            self.fb.set_portrait(request.orientation.is_portrait());
            self.fb.clear(true);
            self.draw_reader_page(request);
            if !self.fb.is_portrait() {
                // Landscape frames mount for the panel; portrait stays
                // viewer-upright, the flush transform owns the quarter-turn.
                self.fb.flip_vertical();
            }
        } else {
            self.draw_shell(request, false);
        }
        self.apply_orientation(request.orientation);
        self.finish_frame(request);
    }

    fn render_sleep(&mut self) {
        let request = self.state.render_request(RenderKind::Page);
        self.draw_shell(request, true);
        self.apply_orientation(request.orientation);
        self.blit();
        self.planner.record_sleep();
        self.last_refresh = RefreshMode::Full as u32 + 1;
        self.frame_seq = self.frame_seq.wrapping_add(1);
    }

    fn apply_orientation(&mut self, orientation: DisplayOrientation) {
        if orientation == DisplayOrientation::LandscapeButtonsTop {
            self.fb.rotate_180();
        }
    }

    fn finish_frame(&mut self, request: app_core::RenderRequest) {
        let mode = self.planner.mode_for(request);
        self.planner.record_render(request, mode);
        self.last_refresh = mode as u32 + 1;
        self.blit();
        self.frame_seq = self.frame_seq.wrapping_add(1);
    }

    /// Everything except the SD reading page goes through the shared shell
    /// renderer, exactly as the firmware's `views::render` does.
    fn draw_shell(&mut self, request: app_core::RenderRequest, sleep: bool) {
        let book_index = ReaderSource::from_book_id(request.book_id)
            .sd_index()
            .unwrap_or(0) as usize
            % SHELF.len();
        let source = &SHELF[book_index];
        let titles: Vec<&str> = SHELF.iter().map(|book| book.title).collect();

        let mut toc: Vec<UiTocItem<'_>> = Vec::new();
        let mut chapter_title = "";
        if self.store_book == Some(book_index as u16) && self.load_status == LoadStatus::Ready {
            if let Some(store) = self.store.as_ref() {
                for chapter in &store.chapters {
                    toc.push(UiTocItem {
                        title: chapter.title.as_str(),
                        level: 1,
                        page: u32::from(chapter.start_page) + 1,
                    });
                }
                if let Some(chapter) = store.chapters.get(request.chapter as usize) {
                    chapter_title = chapter.title.as_str();
                }
            }
        }

        let progress = if request.page_count > 1 {
            (((request.page + 1).min(request.page_count) as u64 * 1000)
                / request.page_count as u64) as u16
        } else {
            0
        };

        let model = UiRenderModel {
            active_book: UiBook {
                title: source.title,
                author: source.author,
                progress_permille: progress,
                cover: None,
            },
            library_status: UiLibraryStatus::Ready,
            library_entries: &titles,
            library_window_start: 0,
            chapters: &toc,
            chapters_window_start: 0,
            chapters_total: toc.len() as u16,
            chapter_title,
            custom_font_name: "",
        };
        if sleep {
            render_shared_sleep(&mut self.fb, request, &model);
        } else {
            render_shared(&mut self.fb, request, &model);
        }
    }

    /// The firmware's SD reading page: body blocks through `ui::reading`,
    /// the page-in-chapter counter in the footer, or the loading book plate
    /// while the pretend card is busy.
    fn draw_reader_page(&mut self, request: app_core::RenderRequest) {
        let book_index = ReaderSource::from_book_id(request.book_id)
            .sd_index()
            .unwrap_or(0);
        let ready = self.store_ready_for(book_index)
            || (self.store_book == Some(book_index) && self.load_status == LoadStatus::Ready);
        if !ready {
            let source = &SHELF[book_index as usize % SHELF.len()];
            // Straddle the frame's vertical center so the plate stays
            // centered on the taller X3 — and on the portrait page.
            let mid = self.fb.height() as i16 / 2;
            draw_centered(&mut self.fb, literata(FontStyle::Bold), source.title, mid - 8);
            draw_centered(&mut self.fb, literata(FontStyle::Italic), source.author, mid + 28);
            return;
        }
        let store = self.store.as_ref().unwrap();
        let page = store.page(request.page);
        ui::reading::draw_reading_page_body(&mut self.fb, store, page);

        let (current, total) = store.chapter_page_position(request.page);
        let label = format!("{}/{}", current, total);
        // Shared with fw::views via ui::reading, which owns the footer's
        // exact frame-relative position in every orientation (`left` tucks
        // it into the mirrored corner for buttons-up).
        ui::reading::draw_reading_page_counter_aligned(
            &mut self.fb,
            &label,
            request.orientation == DisplayOrientation::LandscapeButtonsTop,
        );
        if request.reading_sheet {
            ui::render::render_reading_sheet(&mut self.fb);
        }
    }

    /// Convert the framebuffer to viewer-oriented RGBA. Landscape frames
    /// are panel-mounted (rows mirrored for the X4's upside-down panel) so
    /// they un-flip here; portrait frames are composed viewer-upright and
    /// paint straight through at their swapped dimensions. The RGBA buffer
    /// holds the same pixel count either way.
    fn blit(&mut self) {
        let (width, height) = (self.fb.width(), self.fb.height());
        let portrait = self.fb.is_portrait();
        for y in 0..height {
            let src_y = if portrait { y } else { height - 1 - y };
            for x in 0..width {
                let color = if self.fb.pixel(x, src_y) { PAPER } else { INK };
                let offset = (y * width + x) * 4;
                self.rgba[offset] = color[0];
                self.rgba[offset + 1] = color[1];
                self.rgba[offset + 2] = color[2];
                self.rgba[offset + 3] = 255;
            }
        }
    }
}

fn home_network() -> WifiSsid {
    WifiSsid::new("HOME-WIFI").unwrap()
}

fn draw_centered(fb: &mut Framebuffer, font: &'static display::font::BitmapFont, text: &str, y: i16) {
    let width = measure_text(font, text) as i16;
    let frame_width = fb.width() as i16;
    draw_text(fb, font, text, (frame_width - width) / 2, y, false);
}

/// The desktop emulator's transition-to-storage-command mapping, verbatim.
fn storage_command_for_transition(
    previous: app_core::ReaderState,
    next: app_core::ReaderState,
) -> Option<StorageCommand> {
    let Some(index) = ReaderSource::from_book_id(next.book_id).sd_index() else {
        return None;
    };
    if next.view != AppView::Reading {
        return None;
    }

    if previous.book_id != next.book_id
        || previous.chapter != next.chapter
        || previous.view != AppView::Reading
    {
        return Some(StorageCommand::OpenBook {
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: 5,
            type_settings: next.type_settings(),
        });
    }

    if next.page.saturating_add(2) >= next.sd_page_count {
        return Some(StorageCommand::ExtendSection {
            request_id: 0,
            book_id: next.book_id,
            index,
            chapter: next.chapter,
            target_pages: next.page.saturating_add(5).min(u16::MAX as u32) as u16,
            type_settings: next.type_settings(),
        });
    }

    if previous.page != next.page {
        return Some(StorageCommand::StoreProgress(next.persisted()));
    }

    None
}

// ---------------------------------------------------------------------------
// Raw wasm ABI. Single-threaded by construction; the browser calls in on one
// thread and every export goes through the same static cell.

static mut EMULATOR: Option<WebEmulator> = None;

fn emulator() -> &'static mut WebEmulator {
    unsafe {
        #[allow(static_mut_refs)]
        EMULATOR.get_or_insert_with(WebEmulator::boot)
    }
}

#[no_mangle]
pub extern "C" fn x4_boot() {
    emulator();
}

#[no_mangle]
pub extern "C" fn x4_key(button: u32, now_ms: f64) {
    let button = match button {
        0 => Button::Power,
        1 => Button::Back,
        2 => Button::Confirm,
        3 => Button::Previous,
        4 => Button::Next,
        5 => Button::PagePrevious,
        _ => Button::PageNext,
    };
    emulator().input(button, now_ms);
}

#[no_mangle]
pub extern "C" fn x4_tick(now_ms: f64) {
    emulator().tick(now_ms);
}

#[no_mangle]
pub extern "C" fn x4_frame_ptr() -> *const u8 {
    emulator().rgba.as_ptr()
}

/// Logical width of the current frame: `WIDTH` in landscape, `HEIGHT` when
/// the frame was composed portrait. The page sizes its canvas from these.
#[no_mangle]
pub extern "C" fn x4_frame_width() -> u32 {
    emulator().fb.width() as u32
}

#[no_mangle]
pub extern "C" fn x4_frame_height() -> u32 {
    emulator().fb.height() as u32
}

#[no_mangle]
pub extern "C" fn x4_orientation() -> u32 {
    emulator().state.orientation as u32
}

#[no_mangle]
pub extern "C" fn x4_frame_seq() -> u32 {
    emulator().frame_seq
}

/// Refresh mode of the most recent panel write: 0 none, 1 full, 2 fast,
/// 3 fast-clean (RefreshMode discriminant + 1).
#[no_mangle]
pub extern "C" fn x4_last_refresh() -> u32 {
    emulator().last_refresh
}

#[no_mangle]
pub extern "C" fn x4_sleeping() -> u32 {
    emulator().sleeping as u32
}

#[no_mangle]
pub extern "C" fn x4_snapshot_ptr() -> *const u32 {
    let emu = emulator();
    emu.refresh_snapshot();
    emu.snapshot.as_ptr()
}

#[no_mangle]
pub extern "C" fn x4_restore(
    book_id: u32,
    chapter: u32,
    page: u32,
    orientation: u32,
    policy: u32,
    size: u32,
    spacing: u32,
    weight: u32,
    family: u32,
) {
    emulator().restore([
        book_id, chapter, page, orientation, policy, size, spacing, weight, family,
    ]);
}

// TypeSettings is compared in store_ready_for; keep the import used even in
// builds where inference would otherwise drop it.
#[allow(dead_code)]
fn _settings_witness(settings: TypeSettings) -> TypeSettings {
    settings
}
