use crate::reader_layout::{self, READER_LEFT_X, READER_RIGHT_X};
use crate::reader_store::{BookLoadStatus, LibraryScanStatus, ReaderStore, MAX_LIBRARY_BOOKS};
use crate::{catalog, AppView, ReaderSource, RenderRequest};
use core::fmt::Write;
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, FontStyle};
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::{Rect, WIDTH};
use heapless::String;
use proto::text::TextAlign;
use ui::{
    app_render::{self, UiRenderModel},
    UiBook, UiCover, UiLibraryStatus, UiTocItem,
};

const SHOW_INPUT_DEBUG: bool = false;
const MAX_UI_CHAPTERS: usize = 64;

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    if request.view == AppView::Reading && ReaderSource::from_book_id(request.book_id).is_sd() {
        fb.clear(true);
        draw_sd_reader_page(fb, request, sd_library);
        mirror_framebuffer_long_axis(fb);
    } else {
        let mut library_entries = [""; MAX_LIBRARY_BOOKS];
        let mut chapters = [UiTocItem {
            title: "",
            level: 1,
        }; MAX_UI_CHAPTERS];
        let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
        app_render::render_request(fb, request, &model);
    }

    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
}

pub(crate) fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let mut library_entries = [""; MAX_LIBRARY_BOOKS];
    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
    }; MAX_UI_CHAPTERS];
    let model = ui_model(request, sd_library, &mut library_entries, &mut chapters);
    app_render::render_sleep(fb, request, &model);
}

fn ui_model<'a>(
    request: RenderRequest,
    sd_library: &'a ReaderStore,
    library_entries: &'a mut [&'a str; MAX_LIBRARY_BOOKS],
    chapters: &'a mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
) -> UiRenderModel<'a> {
    let library_count = sd_library.catalog_count().min(library_entries.len());
    for (index, entry) in sd_library
        .catalog_entries()
        .iter()
        .take(library_count)
        .enumerate()
    {
        library_entries[index] =
            if sd_library.loaded_index == Some(index) && !sd_library.title.is_empty() {
                sd_library.title.as_str()
            } else {
                entry.display_name.as_str()
            };
    }
    let chapter_count = fill_chapters(chapters, request, sd_library);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) =
        sd_library.active_book_labels(request.book_id, fallback_book.title, fallback_book.author);

    UiRenderModel {
        active_book: UiBook {
            title,
            author,
            progress_permille: book_progress_permille(request),
            cover: if let Some(cover) = sd_library.selected_cover(request.book_id) {
                Some(UiCover {
                    width: cover.width,
                    height: cover.height,
                    stride: cover.stride,
                    bits: cover.bits,
                })
            } else {
                None
            },
        },
        library_status: ui_library_status(sd_library.status),
        library_entries: &library_entries[..library_count],
        chapters: &chapters[..chapter_count],
    }
}

fn fill_chapters<'a>(
    chapters: &mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
    request: RenderRequest,
    sd_library: &'a ReaderStore,
) -> usize {
    if ReaderSource::from_book_id(request.book_id).is_sd() && sd_library.toc_count() > 0 {
        let count = sd_library.toc_count().min(chapters.len());
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            if let Some(toc_item) = sd_library.toc_item(index) {
                *item = UiTocItem {
                    title: toc_item.title,
                    level: toc_item.level,
                };
            }
        }
        return count;
    }
    if ReaderSource::from_book_id(request.book_id).is_sd() {
        let count = sd_library
            .chapter_count_for_ui()
            .max(1)
            .min(chapters.len() as u8) as usize;
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            *item = UiTocItem {
                title: fallback_chapter_title(index),
                level: 1,
            };
        }
        return count;
    }

    let count = (catalog::chapter_count() as usize).min(chapters.len());
    for (index, item) in chapters.iter_mut().take(count).enumerate() {
        if let Some(chapter) = catalog::chapter_at(index) {
            *item = UiTocItem {
                title: chapter.title,
                level: 1,
            };
        }
    }
    count
}

fn fallback_chapter_title(index: usize) -> &'static str {
    const TITLES: [&str; 24] = [
        "Chapter 1",
        "Chapter 2",
        "Chapter 3",
        "Chapter 4",
        "Chapter 5",
        "Chapter 6",
        "Chapter 7",
        "Chapter 8",
        "Chapter 9",
        "Chapter 10",
        "Chapter 11",
        "Chapter 12",
        "Chapter 13",
        "Chapter 14",
        "Chapter 15",
        "Chapter 16",
        "Chapter 17",
        "Chapter 18",
        "Chapter 19",
        "Chapter 20",
        "Chapter 21",
        "Chapter 22",
        "Chapter 23",
        "Chapter 24",
    ];
    TITLES.get(index).copied().unwrap_or("Chapter")
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
    let selected_book_loaded =
        sd_library.loaded_index == ReaderStore::selected_book_index(request.book_id);
    match (sd_library.reader_status(), selected_book_loaded) {
        (_, false) => {
            draw_ascii(fb, "OPENING EPUB", 20, 72, false);
        }
        (BookLoadStatus::Empty | BookLoadStatus::Loading, _) => {
            draw_ascii(fb, "OPENING EPUB", 20, 72, false);
        }
        (BookLoadStatus::Error, _) => {
            draw_ascii(fb, "COULD NOT OPEN EPUB", 20, 72, false);
            draw_ascii(fb, sd_library.reader_error(), 20, 104, false);
        }
        (BookLoadStatus::Ready, _) => {
            let plan = reader_layout::ReaderPagePlan::new(sd_library, request.page);
            let page_count = plan.page_count().max(1);
            plan.for_each_block(sd_library, |block| {
                let role = block.record.role;
                let align = block.record.align;
                match align {
                    TextAlign::Left => {
                        let x = reader_layout::reader_x_for(role);
                        if block.record.line_count == 1 {
                            reader_layout::draw_styled_line(
                                fb,
                                block.text,
                                x,
                                block.y,
                                block.style,
                            );
                        } else {
                            reader_layout::draw_wrapped_literata(
                                fb,
                                block.font,
                                block.text,
                                x,
                                block.y,
                                reader_layout::reader_max_x_for(role, align),
                                block.advance,
                            );
                        }
                    }
                    TextAlign::Justify => {
                        let x = reader_layout::reader_x_for(role);
                        if block.record.line_count == 1 {
                            reader_layout::draw_styled_line(
                                fb,
                                block.text,
                                x,
                                block.y,
                                block.style,
                            );
                        } else {
                            reader_layout::draw_justified_wrapped_literata(
                                fb,
                                block.font,
                                block.text,
                                x,
                                block.y,
                                reader_layout::reader_max_x_for(role, align),
                                block.advance,
                            );
                        }
                    }
                    TextAlign::Center => {
                        if block.record.line_count == 1 {
                            let width =
                                reader_layout::styled_text_ink_width(block.text, block.font)
                                    .min(READER_RIGHT_X - READER_LEFT_X);
                            let x = ((WIDTH as i16 - width) / 2).max(READER_LEFT_X);
                            reader_layout::draw_styled_line(
                                fb,
                                block.text,
                                x,
                                block.y,
                                block.style,
                            );
                        } else {
                            reader_layout::draw_centered_wrapped_literata(
                                fb,
                                block.font,
                                block.text,
                                block.y,
                                READER_RIGHT_X - READER_LEFT_X,
                                block.advance,
                            );
                        }
                    }
                };
                true
            });
            draw_reader_footer(fb, request, sd_library, page_count);
        }
    }
}

fn draw_reader_footer(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
    page_count: u32,
) {
    let font = literata(FontStyle::Regular);
    let fallback = catalog::active_book(request.book_id);
    let (title, _) = sd_library.active_book_labels(request.book_id, fallback.title, "");

    let section_total = if sd_library.current_section_page_count > 0 {
        sd_library.current_section_page_count as u32
    } else {
        page_count
    }
    .max(1);
    let section_current = if sd_library.current_section_page_count > 0 {
        request
            .page
            .saturating_sub(sd_library.current_section_start_page)
            .saturating_add(1)
    } else {
        request.page.saturating_add(1)
    }
    .min(section_total);

    let mut label = String::<32>::new();
    let _ = write!(label, "{}/{}", section_current, section_total);
    let label_width = measure_text(font, label.as_str()) as i16;
    let footer_y = 477;
    let footer_pad = 16;
    let label_x = READER_RIGHT_X - label_width - footer_pad;
    let title_right = label_x - 14;
    draw_text_centered_truncated_local(fb, font, title, footer_pad, title_right, footer_y);
    draw_text(fb, font, label.as_str(), label_x, footer_y, false);
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

fn mirror_framebuffer_long_axis(fb: &mut Framebuffer) {
    for y in 0..display::HEIGHT / 2 {
        let other_y = display::HEIGHT - 1 - y;
        for x in 0..display::WIDTH {
            let top = fb.pixel(x, y);
            let bottom = fb.pixel(x, other_y);
            fb.set_pixel(x, y, bottom);
            fb.set_pixel(x, other_y, top);
        }
    }
}

fn button_label(button: Option<crate::Button>) -> &'static str {
    match button {
        Some(crate::Button::Power) => "POWER",
        Some(crate::Button::Back) => "BACK",
        Some(crate::Button::Confirm) => "OK",
        Some(crate::Button::Previous) => "PREV",
        Some(crate::Button::Next) => "NEXT",
        None => "NONE",
    }
}

fn book_progress_permille(request: RenderRequest) -> u16 {
    let chapters = catalog::chapter_count().max(1) as u32;
    ((request.chapter as u32 * 1000) / chapters.saturating_sub(1).max(1)) as u16
}
