use crate::reader_layout;
use crate::reader_store::{
    BookLoadStatus, LibraryScanStatus, ReaderStore, LIBRARY_WINDOW, MAX_FIRMWARE_FILES,
};
use crate::{catalog, AppView, DisplayOrientation, ReaderSource, RenderRequest};
use core::fmt::Write;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontFamily, FontStyle};
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::Rect;
use heapless::String;
use ui::{
    app_render::{self, UiRenderModel},
    UiBook, UiCover, UiLibraryStatus, UiTocItem,
};

const SHOW_INPUT_DEBUG: bool = false;
// Decoupled from the 128-entry event/resident caps: the overview reads the
// full chapter list from the card, so the render window must hold it.
const MAX_UI_CHAPTERS: usize = 256;

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    if request.view == AppView::Reading && ReaderSource::from_book_id(request.book_id).is_sd() {
        render_sd_reading_while(fb, request, sd_library, || true);
        return;
    }
    fb.set_frame(app_render::fb_frame(request.orientation));
    let mut library_entries = [""; LIBRARY_WINDOW];
    let mut firmware_entries = [""; MAX_FIRMWARE_FILES];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(
        request,
        sd_library,
        &mut library_entries,
        &mut firmware_entries,
        &mut chapters,
    );
    app_render::render_request(fb, request, &model);

    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
}

/// The reading page of a card book as [`render`] draws it from RAM, asking
/// `go_on` before each line and stopping part way when it says no. False
/// when it stopped, leaving the frame incomplete. [`render`] draws every
/// such page through here, so a page drawn ahead of a press
/// (`crate::next_page`) is the frame the press's own render would draw.
/// That holds because the page reads nothing from `request` beyond the
/// fields of `app_core::ReadingPageKey` (keep that list current when this
/// draw starts reading another field) and nothing else outside the store.
pub(crate) fn render_sd_reading_while(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    go_on: impl FnMut() -> bool,
) -> bool {
    fb.set_frame(app_render::fb_frame(request.orientation));
    fb.clear(true);
    let finished = draw_sd_reader_page(fb, request, sd_library, go_on);
    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
    finished
}

/// Whether `request`'s page can be drawn ahead of the press that will ask
/// for it: a reading page of the loaded card book whose page is resident
/// in the store, laid out as the request says, that [`render`] draws from
/// RAM alone. A page with images, or set in the custom face from the card,
/// draws inside an SD session and is left to its own render, as is any
/// page while the input debug overlay (which draws per-press fields) is
/// compiled in.
#[cfg(not(feature = "device-x3"))]
pub(crate) fn page_can_be_staged(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    let Some(index) = ReaderSource::from_book_id(request.book_id).sd_index() else {
        return false;
    };
    !SHOW_INPUT_DEBUG
        && request.view == AppView::Reading
        && sd_library.covers_global_page(usize::from(index), request.page)
        && reading_buffer_ready(request, sd_library)
        && !reader_reads_card(request, sd_library)
        && !ui::reading::page_has_images(
            sd_library,
            reader_layout::ReaderPagePlan::new(sd_library, request.page).page(),
        )
}

/// Whether a render of `request` draws from the card, inside an SD session:
/// a reading page set in a custom face read from the card, or one whose
/// blocks include images. Every other render draws from RAM ([`render`]).
pub(crate) fn reader_reads_card(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    custom_face_on_card(request, sd_library) || reader_page_has_images(request, sd_library)
}

/// A reading page set in a custom face from the card's font pack. A face
/// compiled into the firmware draws from flash, like the built-in ones.
fn custom_face_on_card(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    request.view == AppView::Reading
        && ReaderSource::from_book_id(request.book_id).is_sd()
        && request.font_family == FontFamily::Custom
        && !display::font::builtin_custom_available()
        && sd_library.custom_font_available()
}

/// A reading page that will draw image blocks, whose rows are in the
/// book's packs. A book without images stops at the first check.
fn reader_page_has_images(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    sd_library.book_has_images()
        && request.view == AppView::Reading
        && ReaderSource::from_book_id(request.book_id).is_sd()
        && sd_library.reader_status() == BookLoadStatus::Ready
        && reading_buffer_ready(request, sd_library)
        && ui::reading::page_has_images(
            sd_library,
            reader_layout::ReaderPagePlan::new(sd_library, request.page).page(),
        )
}

/// Draws a page that [`reader_reads_card`], inside the SD session `root`
/// belongs to, streaming its images through `band`, a buffer idle while a
/// page draws. False, drawing nothing, for any other render.
pub(crate) fn render_reader_from_root(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    root: &crate::sd_session::SdRoot<'_>,
    band: &mut [u8],
) -> bool {
    if !reader_reads_card(request, sd_library) {
        return false;
    }

    fb.set_frame(app_render::fb_frame(request.orientation));
    fb.clear(true);
    draw_sd_reader_page_from_card(fb, request, sd_library, font_metrics, root, band);
    true
}

pub(crate) fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let mut library_entries = [""; LIBRARY_WINDOW];
    let mut firmware_entries = [""; MAX_FIRMWARE_FILES];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(
        request,
        sd_library,
        &mut library_entries,
        &mut firmware_entries,
        &mut chapters,
    );
    app_render::render_sleep(fb, request, &model);
}

pub(crate) fn render_sleep_blank(fb: &mut Framebuffer) {
    fb.set_frame(display::fb::FbFrame::Native);
    fb.clear(true);
    draw_text_centered_truncated_local(
        fb,
        literata(FontStyle::Regular),
        "MarigoldOS",
        0,
        display::WIDTH as i16,
        display::HEIGHT as i16 / 2,
    );
}

fn ui_model<'a>(
    request: RenderRequest,
    sd_library: &'a ReaderStore,
    library_entries: &'a mut [&'a str; LIBRARY_WINDOW],
    firmware_entries: &'a mut [&'a str; MAX_FIRMWARE_FILES],
    chapters: &'a mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
) -> UiRenderModel<'a> {
    // The resident list window over the streamed catalog: `library_entries[i]`
    // is the book at absolute index `window_start + i`. The firmware refills
    // this window from the card before each Library render.
    let window = sd_library.catalog_window();
    let window_start = sd_library.catalog_window_start();
    let library_count = window.len().min(library_entries.len());
    for (i, entry) in window.iter().take(library_count).enumerate() {
        let absolute = window_start + i;
        library_entries[i] =
            if sd_library.loaded_index == Some(absolute) && !sd_library.title.is_empty() {
                sd_library.title.as_str()
            } else {
                entry.display_label.as_str()
            };
    }
    let firmware_count = sd_library
        .firmware_files()
        .len()
        .min(firmware_entries.len());
    for (index, entry) in sd_library
        .firmware_files()
        .iter()
        .take(firmware_count)
        .enumerate()
    {
        firmware_entries[index] = entry.display_name.as_str();
    }
    let (chapter_count, chapters_window_start, chapters_total) =
        fill_chapters(chapters, request, sd_library);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback_book.title, fallback_book.author);

    // The firmware-resolved current-chapter title, shown only for the book it
    // was resolved for -- so a colophon never names another book's chapter.
    // Keyed on source identity (not the loaded book) so it survives boot
    // restore, where the title is read before the book is opened.
    let chapter_title = if !sd_library.current_chapter_title().is_empty()
        && sd_library.current_chapter_source() == sd_library.source_identity(request.book_id)
    {
        sd_library.current_chapter_title()
    } else {
        ""
    };

    UiRenderModel {
        active_book: UiBook {
            title,
            author,
            progress_permille: book_progress_permille(request),
            cover: sd_library
                .selected_cover(request.book_id)
                .map(|cover| UiCover {
                    width: cover.width,
                    height: cover.height,
                    stride: cover.stride,
                    bits: cover.bits,
                }),
        },
        library_status: ui_library_status(sd_library.status),
        library_entries: &library_entries[..library_count],
        library_window_start: window_start as u16,
        firmware_entries: &firmware_entries[..firmware_count],
        chapters: &chapters[..chapter_count],
        chapters_window_start: chapters_window_start as u16,
        chapters_total: chapters_total.min(u16::MAX as usize) as u16,
        chapter_title,
        custom_font_name: sd_library.custom_font_name(),
    }
}

/// Fill the UI chapter rows and return `(resident_len, window_start, total)`
/// — the Contents page draws absolute indices out of this resident window.
fn fill_chapters<'a>(
    chapters: &mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
    request: RenderRequest,
    sd_library: &'a ReaderStore,
) -> (usize, usize, usize) {
    // The on-disk chapter list, windowed from the card into the section
    // buffer while the overview is open: row `i` is absolute chapter
    // `window_start + i`.
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.text_holds_toc() {
        let total = sd_library.overview_chapter_count();
        let window_start = sd_library.toc_window_start();
        let count = total.saturating_sub(window_start).min(chapters.len());
        for (offset, item) in chapters.iter_mut().take(count).enumerate() {
            let index = window_start + offset;
            *item = UiTocItem {
                title: sd_library.overview_title_at(index),
                level: sd_library.overview_level_at(index),
                page: u32::from(sd_library.overview_page_at(index)),
            };
        }
        return (count, window_start, total);
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.toc_count() > 0 {
        let count = sd_library.toc_count().min(chapters.len());
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            if let Some(toc_item) = sd_library.toc_item(index) {
                *item = UiTocItem {
                    title: toc_item.title,
                    level: toc_item.level,
                    page: toc_item.page,
                };
            }
        }
        return (count, 0, count);
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() {
        let count = sd_library
            .chapter_count_for_ui()
            .max(1)
            .min(chapters.len() as u16) as usize;
        for item in chapters.iter_mut().take(count) {
            // Empty titles render as numbered chapters in the contents view.
            *item = UiTocItem {
                title: "",
                level: 1,
                page: 0,
            };
        }
        return (count, 0, count);
    }

    let count = (catalog::chapter_count() as usize).min(chapters.len());
    for (index, item) in chapters.iter_mut().take(count).enumerate() {
        if let Some(chapter) = catalog::chapter_at(index) {
            *item = UiTocItem {
                title: chapter.title,
                level: 1,
                page: 0,
            };
        }
    }
    (count, 0, count)
}

fn ui_library_status(status: LibraryScanStatus) -> UiLibraryStatus {
    match status {
        LibraryScanStatus::NotScanned => UiLibraryStatus::NotScanned,
        LibraryScanStatus::Scanning => UiLibraryStatus::Scanning,
        LibraryScanStatus::Ready => UiLibraryStatus::Ready,
        LibraryScanStatus::Empty => UiLibraryStatus::Empty,
        LibraryScanStatus::Error => UiLibraryStatus::Error,
    }
}

/// Whether the store holds `request`'s page ready to draw; until it does, a
/// reading render shows the bookplate.
fn reading_buffer_ready(request: RenderRequest, sd_library: &ReaderStore) -> bool {
    // A store paginated under different type settings is as unusable as an
    // unloaded one; show the loading plate until the relayout lands.
    let layout_current = sd_library.type_settings() == request.type_settings();
    // While the Chapters overview borrows the reading text buffer for the
    // on-disk TOC, page/block records still point at the old section but the
    // bytes underneath are TOC records -- drawing the body now paints garbage
    // glyphs. Hold the bookplate until the reading section reloads on exit.
    // The page box is part of the layout: blocks wrapped for the portrait
    // column must not paint into a landscape frame (they cover half the
    // panel); hold the bookplate until the rebuild lands.
    layout_current
        && sd_library.portrait() == app_core::is_portrait(request.orientation)
        && sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id)
        && !sd_library.text_holds_toc()
}

/// False when `go_on` stopped the page body part way.
fn draw_sd_reader_page(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    go_on: impl FnMut() -> bool,
) -> bool {
    match (
        sd_library.reader_status(),
        reading_buffer_ready(request, sd_library),
    ) {
        (_, false) | (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_sd_reader_loading(fb, request, sd_library);
        }
        (BookLoadStatus::Error, _) => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = reader_layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            if !ui::reading::draw_reading_page_body_while(
                fb,
                sd_library,
                plan.page(),
                ui::reading::draw_image_placeholder,
                go_on,
            ) {
                return false;
            }
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
    true
}

/// [`draw_sd_reader_page`] inside an SD session: the body in the custom
/// face when it is the one chosen, and images from the book's packs.
fn draw_sd_reader_page_from_card(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    font_metrics: &mut crate::custom_font::MetricCache,
    root: &crate::sd_session::SdRoot<'_>,
    band: &mut [u8],
) {
    match (
        sd_library.reader_status(),
        reading_buffer_ready(request, sd_library),
    ) {
        (_, false) | (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_sd_reader_loading(fb, request, sd_library);
        }
        (BookLoadStatus::Error, _) => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = reader_layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            let page = plan.page();
            let custom = custom_face_on_card(request, sd_library);
            crate::page_images::draw_page(
                root,
                sd_library,
                request.book_id,
                page,
                band,
                clock_micros,
                |images| {
                    if custom
                        && crate::custom_font::draw_reading_page_body(
                            root,
                            fb,
                            sd_library,
                            font_metrics,
                            page,
                            &mut *images,
                        )
                    {
                        return;
                    }
                    if custom {
                        // The custom face may have painted part of the page
                        // before failing; start the built-in fallback from a
                        // clean frame.
                        fb.clear(true);
                    }
                    draw_built_in_body(fb, sd_library, page, images);
                },
            );
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
}

/// The page body in a built-in face, out of line so that a page in the
/// custom face does not carry this draw's frame as well as its own.
#[inline(never)]
fn draw_built_in_body(
    fb: &mut Framebuffer,
    sd_library: &ReaderStore,
    page: proto::cache::PageRecord,
    images: &mut crate::page_images::ImageDrawer<'_>,
) {
    ui::reading::draw_reading_page_body(fb, sd_library, page, images);
}

fn clock_micros() -> u64 {
    embassy_time::Instant::now().as_micros()
}

/// The reading page is full bleed; its only resident furniture is the
/// page-in-chapter counter, set in the 16px apparatus size.
fn draw_reader_footer(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    page_count: u32,
) {
    // Page within the chapter (spine item), aggregated across its cache
    // sections. Falls back to the current section, then the whole book, when
    // no book index is loaded to aggregate from.
    let (chapter_current, chapter_total) = sd_library
        .chapter_page_position(request.page)
        .unwrap_or_else(|| {
            let total = if sd_library.current_section_page_count > 0 {
                sd_library.current_section_page_count as u32
            } else {
                page_count
            }
            .max(1);
            let current = if sd_library.current_section_page_count > 0 {
                request
                    .page
                    .saturating_sub(sd_library.current_section_start_page)
                    .saturating_add(1)
            } else {
                request.page.saturating_add(1)
            }
            .min(total);
            (current, total)
        });

    let mut label = String::<48>::new();
    // A book cut short by a capacity limit (spine or section count) says so
    // on its last page instead of simply stopping.
    let truncated_end = sd_library.book_cache_partial
        && sd_library.book_next_spine().is_none()
        && sd_library.book_total_pages > 0
        && request.page.saturating_add(1) >= sd_library.book_total_pages;
    if truncated_end {
        let _ = write!(
            label,
            "{}/{} \u{b7} book cut short",
            chapter_current, chapter_total
        );
    } else {
        let _ = write!(label, "{}/{}", chapter_current, chapter_total);
    }
    ui::reading::draw_reading_page_counter_aligned(
        fb,
        label.as_str(),
        request.orientation == DisplayOrientation::LandscapeButtonsTop,
    );
}

fn draw_sd_reader_loading(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let fallback = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback.title, fallback.author);
    ui::reading::draw_reading_bookplate(fb, title, author);
}

fn draw_sd_reader_error(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let fallback = catalog::active_book(request.book_id);
    let (title, _) = sd_library.active_book_labels(request.book_id, fallback.title, "");

    let left = 8;
    let right = fb.frame_width() as i16 - 8;
    let title_y = fb.frame_height() as i16 / 2 - 16;
    let message_y = title_y + 40;
    draw_text_centered_truncated_local(fb, title_font, title, left, right, title_y);
    let error = sd_library.reader_error();
    let message: &str = if error.is_empty() {
        "Could not open this book."
    } else {
        error
    };
    draw_text_centered_truncated_local(fb, body_font, message, left, right, message_y);
}

fn draw_text_centered_truncated_local(
    fb: &mut Framebuffer,
    font: &'static display::font::BitmapFont,
    text: &str,
    left: i16,
    right: i16,
    y: i16,
) {
    let max_width = (right - left).max(0);
    if max_width == 0 {
        return;
    }
    let width = measure_text(font, text) as i16;
    if width <= max_width {
        draw_text(fb, font, text, left + (max_width - width) / 2, y, false);
        return;
    }

    let mut end = text.len();
    while end > 0 {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = &text[..end];
        let candidate_width = measure_text(font, candidate) as i16;
        let ellipsis_width = measure_text(font, "...") as i16;
        if candidate_width + ellipsis_width <= max_width {
            let x = left + (max_width - candidate_width - ellipsis_width) / 2;
            draw_text(fb, font, candidate, x, y, false);
            draw_text(fb, font, "...", x + candidate_width, y, false);
            return;
        }
        end = end.saturating_sub(1);
    }
}

fn draw_input_sample(fb: &mut Framebuffer, request: RenderRequest) {
    fill_rect(fb, Rect::new(488, 104, 220, 64), true);
    stroke_rect(fb, Rect::new(488, 104, 220, 64), false);
    draw_ascii(fb, "LAST", 504, 120, false);
    draw_ascii(fb, button_label(request.last_button), 552, 120, false);
}

fn button_label(button: Option<crate::Button>) -> &'static str {
    match button {
        Some(crate::Button::Power) => "POWER",
        Some(crate::Button::Back) => "BACK",
        Some(crate::Button::Confirm) => "OK",
        Some(crate::Button::Previous) => "PREV",
        Some(crate::Button::Next) => "NEXT",
        Some(crate::Button::PagePrevious) => "PAGE-",
        Some(crate::Button::PageNext) => "PAGE+",
        None => "NONE",
    }
}

fn book_progress_permille(request: RenderRequest) -> u16 {
    let chapters = catalog::chapter_count().max(1) as u32;
    ((request.chapter as u32 * 1000) / chapters.saturating_sub(1).max(1)) as u16
}
