use crate::reader_layout::{self, READER_LEFT_X, READER_RIGHT_X};
use crate::reader_store::{BookLoadStatus, LibraryScanStatus, ReaderStore, LIBRARY_WINDOW};
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
        fb.clear(true);
        draw_sd_reader_page(fb, request, sd_library);
        fb.flip_vertical();
    } else {
        let mut library_entries = [""; LIBRARY_WINDOW];
        let mut chapters = [UiTocItem {
            title: "",
            level: 1,
            page: 0,
        }; MAX_UI_CHAPTERS];
        let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
        app_render::render_request(fb, request, &model);
    }

    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
    apply_orientation(fb, request.orientation);
}

pub(crate) fn render_custom_reader_from_root(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    root: &crate::sd_session::SdRoot<'_>,
) -> bool {
    if request.view != AppView::Reading
        || !ReaderSource::from_book_id(request.book_id).is_sd()
        || request.font_family != FontFamily::Custom
        || !sd_library.custom_font_available()
    {
        return false;
    }

    fb.clear(true);
    draw_sd_reader_page_with_custom_font(fb, request, sd_library, root);
    fb.flip_vertical();
    apply_orientation(fb, request.orientation);
    true
}

pub(crate) fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let mut library_entries = [""; LIBRARY_WINDOW];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
        page: 0,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    app_render::render_sleep(fb, request, &model);
    apply_orientation(fb, request.orientation);
}

pub(crate) fn render_sleep_blank(fb: &mut Framebuffer) {
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

fn apply_orientation(fb: &mut Framebuffer, orientation: DisplayOrientation) {
    if orientation == DisplayOrientation::LandscapeButtonsTop {
        fb.rotate_180();
    }
}

fn ui_model<'a>(
    request: RenderRequest,
    sd_library: &'a ReaderStore,
    library_entries: &'a mut [&'a str; LIBRARY_WINDOW],
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
            .min(chapters.len() as u8) as usize;
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

fn draw_sd_reader_page(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    // A store paginated under different type settings is as unusable as an
    // unloaded one; show the loading plate until the relayout lands.
    let layout_current = sd_library.type_settings()
        == display::font::TypeSettings {
            size: request.font_size,
            spacing: request.line_spacing,
            weight: request.font_weight,
            family: request.font_family,
        };
    // While the Chapters overview borrows the reading text buffer for the
    // on-disk TOC, page/block records still point at the old section but the
    // bytes underneath are TOC records -- drawing the body now paints garbage
    // glyphs. Hold the bookplate until the reading section reloads on exit.
    let reading_buffer_ready = layout_current
        && sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id)
        && !sd_library.text_holds_toc();
    match (sd_library.reader_status(), reading_buffer_ready) {
        (_, false) | (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_sd_reader_loading(fb, request, sd_library);
        }
        (BookLoadStatus::Error, _) => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = reader_layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            ui::reading::draw_reading_page_body(fb, sd_library, plan.page());
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
}

fn draw_sd_reader_page_with_custom_font(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    root: &crate::sd_session::SdRoot<'_>,
) {
    let layout_current = sd_library.type_settings()
        == display::font::TypeSettings {
            size: request.font_size,
            spacing: request.line_spacing,
            weight: request.font_weight,
            family: request.font_family,
        };
    let reading_buffer_ready = layout_current
        && sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id)
        && !sd_library.text_holds_toc();
    match (sd_library.reader_status(), reading_buffer_ready) {
        (_, false) | (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_sd_reader_loading(fb, request, sd_library);
        }
        (BookLoadStatus::Error, _) => {
            draw_sd_reader_error(fb, request, sd_library);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = reader_layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            if !crate::custom_font::draw_reading_page_body(root, fb, sd_library, plan.page()) {
                ui::reading::draw_reading_page_body(fb, sd_library, plan.page());
            }
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
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

    let mut label = String::<32>::new();
    let _ = write!(label, "{}/{}", chapter_current, chapter_total);
    ui::reading::draw_reading_page_counter_aligned(
        fb,
        label.as_str(),
        request.orientation == DisplayOrientation::LandscapeButtonsTop,
    );
}

fn draw_sd_reader_loading(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let title_font = literata(FontStyle::Bold);
    let author_font = literata(FontStyle::Italic);
    let fallback = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback.title, fallback.author);

    // Vertically center title + author block within the reader page region
    // (panel-relative, so the X3's taller page carries the plate down).
    let title_y = display::HEIGHT as i16 / 2 - 8;
    let author_y = title_y + 36;
    draw_text_centered_truncated_local(
        fb,
        title_font,
        title,
        READER_LEFT_X,
        READER_RIGHT_X,
        title_y,
    );
    if !author.is_empty() {
        draw_text_centered_truncated_local(
            fb,
            author_font,
            author,
            READER_LEFT_X,
            READER_RIGHT_X,
            author_y,
        );
    }
}

fn draw_sd_reader_error(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let fallback = catalog::active_book(request.book_id);
    let (title, _) = sd_library.active_book_labels(request.book_id, fallback.title, "");

    let title_y = 224i16;
    let message_y = title_y + 40;
    draw_text_centered_truncated_local(
        fb,
        title_font,
        title,
        READER_LEFT_X,
        READER_RIGHT_X,
        title_y,
    );
    let error = sd_library.reader_error();
    let message: &str = if error.is_empty() {
        "Could not open this book."
    } else {
        error
    };
    draw_text_centered_truncated_local(
        fb,
        body_font,
        message,
        READER_LEFT_X,
        READER_RIGHT_X,
        message_y,
    );
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
