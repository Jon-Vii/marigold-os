use crate::reader_layout::{self, READER_LEFT_X, READER_RIGHT_X};
use crate::reader_store::{
    BookLoadStatus, LibraryScanStatus, ReaderStore, COVER_STRIDE, MAX_LIBRARY_BOOKS,
};
use crate::{catalog, AppView, DisplayOrientation, RefreshPolicy, RenderRequest};
use display::fb::Framebuffer;
use display::font::{draw_text, literata, FontStyle};
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::{Rect, HEIGHT, WIDTH};
use heapless::String;
use proto::text::TextAlign;
use ui::{
    render::render_shell_overlay, UiBook, UiCover, UiLibraryStatus, UiOrientation, UiRefreshPolicy,
    UiShell, UiTocItem, UiView,
};

const SHOW_INPUT_DEBUG: bool = false;
const MAX_UI_CHAPTERS: usize = 64;
const HOME_ITEMS: [&str; 4] = ["READ", "FILES", "SYNC", "SETTINGS"];
const SETTINGS_ITEMS: [&str; 3] = ["ORIENTATION", "REFRESH", "BACK TO HOME"];

pub(crate) fn render(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    fb.clear(true);
    render_reader_shell(fb, request, sd_library);
    if SHOW_INPUT_DEBUG {
        draw_input_sample(fb, request);
    }
}

pub(crate) fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    fb.clear(true);
    stroke_rect(fb, Rect::new(0, 0, WIDTH as u16, HEIGHT as u16), false);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) = active_book_labels(
        request,
        sd_library,
        fallback_book.title,
        fallback_book.author,
    );

    draw_ascii(fb, "SLEEPING", 360, 144, false);
    draw_ascii_centered(fb, title, 216);
    draw_ascii_centered(fb, author, 248);
    draw_progress_bar(
        fb,
        Rect::new(252, 286, 296, 6),
        book_progress_permille(request),
    );
    draw_ascii(fb, "PRESS POWER TO WAKE", 320, 340, false);

    let mut percent = [0u8; 10];
    draw_ascii(
        fb,
        fmt_percent(request.battery_percent, &mut percent),
        688,
        28,
        false,
    );
    draw_battery_icon(fb, 736, 26, battery_bars(request.battery_percent));
}

fn draw_input_sample(fb: &mut Framebuffer, request: RenderRequest) {
    fill_rect(fb, Rect::new(488, 104, 220, 64), true);
    stroke_rect(fb, Rect::new(488, 104, 220, 64), false);
    draw_ascii(fb, "LAST", 504, 120, false);
    draw_ascii(fb, button_label(request.last_button), 552, 120, false);

    if SHOW_INPUT_DEBUG {
        let mut aux_buf = [0u8; 10];
        let mut nav_buf = [0u8; 10];
        let mut page_buf = [0u8; 10];
        draw_ascii(fb, "GPIO0", 504, 144, false);
        draw_ascii(
            fb,
            fmt_u32(request.aux_raw as u32, &mut aux_buf),
            568,
            144,
            false,
        );
        draw_ascii(fb, "GPIO1", 504, 168, false);
        draw_ascii(
            fb,
            fmt_u32(request.nav_raw as u32, &mut nav_buf),
            568,
            168,
            false,
        );
        draw_ascii(fb, "GPIO2", 504, 192, false);
        draw_ascii(
            fb,
            fmt_u32(request.page_raw as u32, &mut page_buf),
            568,
            192,
            false,
        );
    }
}

pub(crate) fn render_reader_shell(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
) {
    if is_shell_view(request.view) {
        render_shared_shell(fb, request, sd_library);
        return;
    }

    if request.view == AppView::Reading {
        render_reading_landscape(fb, request, sd_library);
        return;
    }

    stroke_rect(fb, Rect::new(0, 0, WIDTH as u16, HEIGHT as u16), false);
    draw_header(fb, request);
    draw_body(fb, request, sd_library);
    draw_footer(fb, request);
}

fn is_shell_view(view: AppView) -> bool {
    matches!(
        view,
        AppView::Home | AppView::Library | AppView::Chapters | AppView::Sync | AppView::Settings
    )
}

fn render_shared_shell(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let mut library_entries = [""; MAX_LIBRARY_BOOKS];
    let library_count = sd_library.count.min(library_entries.len());
    for (index, entry) in sd_library.entries.iter().take(library_count).enumerate() {
        library_entries[index] = entry.display_name.as_str();
    }

    let mut chapters = [UiTocItem {
        title: "",
        level: 1,
    }; MAX_UI_CHAPTERS];
    let chapter_count = fill_chapters(&mut chapters, request, sd_library);

    let fallback_book = catalog::active_book(request.book_id);
    let (title, author) =
        if request.book_id >= 2 && sd_library.reader_status == BookLoadStatus::Ready {
            let title = if sd_library.title.is_empty() {
                fallback_book.title
            } else {
                sd_library.title.as_str()
            };
            let author = if sd_library.author.is_empty() {
                fallback_book.author
            } else {
                sd_library.author.as_str()
            };
            (title, author)
        } else {
            (fallback_book.title, fallback_book.author)
        };

    let shell = UiShell {
        view: ui_view(request.view),
        orientation: ui_orientation(request.orientation),
        refresh_policy: ui_refresh_policy(request.refresh_policy),
        selection: request.selection,
        battery_percent: request.battery_percent,
        active_book: UiBook {
            title,
            author,
            progress_permille: book_progress_permille(request),
            cover: if request.book_id >= 2
                && sd_library.current_index
                    == request.book_id.checked_sub(2).map(|index| index as usize)
                && sd_library.cover_ready
            {
                Some(UiCover {
                    width: sd_library.cover_width,
                    height: sd_library.cover_height,
                    stride: COVER_STRIDE as u16,
                    bits: &sd_library.cover_bits,
                })
            } else {
                None
            },
        },
        library_status: ui_library_status(sd_library.status),
        library_entries: &library_entries[..library_count],
        chapters: &chapters[..chapter_count],
    };
    render_shell_overlay(fb, &shell);
}

fn active_book_labels<'a>(
    request: RenderRequest,
    sd_library: &'a ReaderStore,
    fallback_title: &'a str,
    fallback_author: &'a str,
) -> (&'a str, &'a str) {
    if request.book_id < 2 {
        return (fallback_title, fallback_author);
    }
    if sd_library.reader_status == BookLoadStatus::Ready
        && sd_library.loaded_index == request.book_id.checked_sub(2).map(|index| index as usize)
    {
        let title = if sd_library.title.is_empty() {
            fallback_title
        } else {
            sd_library.title.as_str()
        };
        let author = if sd_library.author.is_empty() {
            fallback_author
        } else {
            sd_library.author.as_str()
        };
        return (title, author);
    }
    request
        .book_id
        .checked_sub(2)
        .and_then(|index| sd_library.entries.get(index as usize))
        .map(|entry| (entry.display_name.as_str(), ""))
        .unwrap_or((fallback_title, fallback_author))
}

fn fill_chapters<'a>(
    chapters: &mut [UiTocItem<'a>; MAX_UI_CHAPTERS],
    request: RenderRequest,
    sd_library: &'a ReaderStore,
) -> usize {
    if request.book_id >= 2 && sd_library.toc_count > 0 {
        let count = sd_library.toc_count.min(chapters.len());
        for (index, item) in chapters.iter_mut().take(count).enumerate() {
            *item = UiTocItem {
                title: sd_library.toc_title(index),
                level: sd_library.toc[index].level.max(1),
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

fn ui_view(view: AppView) -> UiView {
    match view {
        AppView::Home => UiView::Home,
        AppView::Library => UiView::Library,
        AppView::Reading => UiView::Home,
        AppView::Chapters => UiView::Chapters,
        AppView::Sync => UiView::Sync,
        AppView::Settings => UiView::Settings,
    }
}

fn ui_orientation(orientation: DisplayOrientation) -> UiOrientation {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => UiOrientation::LandscapeButtonsBottom,
        DisplayOrientation::LandscapeButtonsTop => UiOrientation::LandscapeButtonsTop,
        DisplayOrientation::PortraitButtonsLeft => UiOrientation::PortraitButtonsLeft,
        DisplayOrientation::PortraitButtonsRight => UiOrientation::PortraitButtonsRight,
    }
}

fn ui_refresh_policy(policy: RefreshPolicy) -> UiRefreshPolicy {
    match policy {
        RefreshPolicy::FastOnly => UiRefreshPolicy::FastOnly,
        RefreshPolicy::FullOnWake => UiRefreshPolicy::FullOnWake,
        RefreshPolicy::FullEveryTen => UiRefreshPolicy::FullEveryTen,
    }
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

pub(crate) fn render_reading_landscape(
    fb: &mut Framebuffer,
    request: RenderRequest,
    sd_library: &ReaderStore,
) {
    if request.book_id < 2 {
        draw_builtin_reader_probe(fb, request);
        return;
    }
    draw_reader_page(fb, request, sd_library);
}

fn draw_builtin_reader_probe(fb: &mut Framebuffer, request: RenderRequest) {
    draw_ascii(fb, "READ MODE", 64, 96, false);
    draw_ascii(fb, "BUILT-IN FALLBACK", 64, 136, false);
    draw_ascii(fb, "BACK RETURNS HOME", 64, 176, false);
    let mut page_buf = [0u8; 10];
    draw_ascii(fb, "CHAPTER", 64, 232, false);
    draw_ascii(
        fb,
        fmt_u32(request.chapter as u32 + 1, &mut page_buf),
        160,
        232,
        false,
    );
    mirror_framebuffer_long_axis(fb);
}

fn draw_header(fb: &mut Framebuffer, request: RenderRequest) {
    draw_ascii(fb, "XTEINK X4", 32, 28, false);
    draw_ascii(fb, view_label(request.view), 328, 28, false);
    draw_ascii(fb, orientation_label(request.orientation), 576, 28, false);
    draw_rule(fb, 64);
}

fn draw_body(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    match request.view {
        AppView::Home => draw_home(fb, request),
        AppView::Library => draw_library_landscape(fb, request),
        AppView::Reading => {}
        AppView::Chapters => draw_chapters(fb, request, sd_library),
        AppView::Sync => {}
        AppView::Settings => draw_settings(fb, request),
    }
}

fn draw_footer(fb: &mut Framebuffer, request: RenderRequest) {
    if request.view == AppView::Reading {
        draw_reading_footer(fb, request);
    } else {
        draw_rule(fb, 408);
        draw_ascii(fb, "PREV NEXT", 32, 432, false);
        draw_ascii(fb, "OK SELECT", 344, 432, false);
        draw_ascii(fb, "BACK", 656, 432, false);
    }
}

fn draw_ascii_centered(fb: &mut Framebuffer, text: &str, y: usize) {
    draw_ascii(fb, text, centered_x_for(WIDTH, text), y, false);
}

fn draw_home(fb: &mut Framebuffer, request: RenderRequest) {
    let book = catalog::active_book(request.book_id);
    draw_cover_placeholder(fb, 92, 112, 184, 248);
    draw_ascii(fb, book.title, 344, 116, false);
    draw_ascii(fb, book.author, 344, 144, false);
    draw_progress_bar(fb, Rect::new(344, 188, 320, 10), 420);
    draw_ascii(
        fb,
        catalog::chapter_at(request.selection as usize)
            .map(|chapter| chapter.title)
            .unwrap_or("Chapter"),
        344,
        216,
        false,
    );

    let mut y = 268;
    for (index, item) in HOME_ITEMS.iter().enumerate() {
        let selected = index == request.selection as usize;
        if selected {
            fill_rect(fb, Rect::new(332, y as u16 - 10, 300, 28), false);
        }
        draw_ascii(fb, if selected { ">" } else { " " }, 348, y, selected);
        draw_ascii(fb, item, 380, y, selected);
        y += 40;
    }
}

fn draw_cover_placeholder(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect(fb, Rect::new(x, y, w, h), false);
    stroke_rect(fb, Rect::new(x + 8, y + 8, w - 16, h - 16), false);
    fill_rect(fb, Rect::new(x + 24, y + 34, 2, h - 68), false);
    draw_ascii(fb, "XTEINK", x as usize + 64, y as usize + 88, false);
    draw_ascii(fb, "BRING UP", x as usize + 48, y as usize + 120, false);
    draw_ascii(fb, "NOTES", x as usize + 72, y as usize + 152, false);
}

fn draw_reader_page(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    if request.book_id >= 2 {
        draw_sd_reader_page(fb, request, sd_library);
        return;
    }

    let index = (request.chapter as usize) % catalog::READER_PAGES.len();
    let page = catalog::READER_PAGES[index];
    let mut baseline_y = 64i16;

    for line in page.iter().take(6) {
        let (style, x, advance) = match line.style {
            catalog::ReaderLineStyle::Heading => (FontStyle::Bold, 18, 34),
            catalog::ReaderLineStyle::Body => (FontStyle::Regular, 20, 30),
            catalog::ReaderLineStyle::Italic => (FontStyle::Italic, 20, 30),
            catalog::ReaderLineStyle::Bold => (FontStyle::Bold, 20, 30),
            catalog::ReaderLineStyle::Quote => (FontStyle::Italic, 46, 30),
        };
        draw_text(fb, literata(style), line.text, x, baseline_y, false);
        baseline_y += advance + line.gap_after as i16;
        if baseline_y > 420 {
            break;
        }
    }
    mirror_framebuffer_long_axis(fb);
}

fn draw_sd_reader_page(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    match sd_library.reader_status {
        BookLoadStatus::Empty | BookLoadStatus::Loading => {
            draw_ascii(fb, "OPENING EPUB", 20, 72, false);
        }
        BookLoadStatus::Error => {
            draw_ascii(fb, "COULD NOT OPEN EPUB", 20, 72, false);
            draw_ascii(fb, &sd_library.error, 20, 104, false);
        }
        BookLoadStatus::Ready => {
            let page_top = 22i16;
            let page_bottom = 472i16;
            let page_count = reader_layout::reader_page_count(sd_library, page_top, page_bottom);
            let requested_page = request.page.min(page_count - 1) as usize;
            let page =
                reader_layout::reader_page_at(sd_library, requested_page, page_top, page_bottom);
            let mut y = page_bottom - 8;

            for offset in 0..page.block_count as usize {
                let index = page.first_block as usize + offset;
                let Some(record) = sd_library.blocks.get(index).copied() else {
                    break;
                };
                let role = record.role;
                let align = record.align;
                let text = sd_library.block_text(index);
                let advance = reader_layout::line_advance_for(role);
                let font = literata(sd_library.block_styles[index]);
                if y < page_top {
                    break;
                }

                match align {
                    TextAlign::Left => {
                        let x = reader_layout::reader_x_for(role);
                        if record.line_count == 1 {
                            reader_layout::draw_styled_line(
                                fb,
                                text,
                                x,
                                y,
                                sd_library.block_styles[index],
                            );
                        } else {
                            reader_layout::draw_wrapped_literata(
                                fb,
                                font,
                                text,
                                x,
                                y,
                                reader_layout::reader_max_x_for(role, align),
                                advance,
                            );
                        }
                    }
                    TextAlign::Justify => {
                        let x = reader_layout::reader_x_for(role);
                        if record.line_count == 1 {
                            reader_layout::draw_styled_line(
                                fb,
                                text,
                                x,
                                y,
                                sd_library.block_styles[index],
                            );
                        } else {
                            reader_layout::draw_justified_wrapped_literata(
                                fb,
                                font,
                                text,
                                x,
                                y,
                                reader_layout::reader_max_x_for(role, align),
                                advance,
                            );
                        }
                    }
                    TextAlign::Center => {
                        if record.line_count == 1 {
                            let width = reader_layout::styled_text_ink_width(text, font)
                                .min(READER_RIGHT_X - READER_LEFT_X);
                            let x = ((WIDTH as i16 - width) / 2).max(READER_LEFT_X);
                            reader_layout::draw_styled_line(
                                fb,
                                text,
                                x,
                                y,
                                sd_library.block_styles[index],
                            );
                        } else {
                            reader_layout::draw_centered_wrapped_literata(
                                fb,
                                font,
                                text,
                                y,
                                READER_RIGHT_X - READER_LEFT_X,
                                advance,
                            );
                        }
                    }
                };
                y -= advance + reader_layout::paragraph_gap_after(sd_library, index);
            }
        }
    }
}

fn draw_reading_footer(fb: &mut Framebuffer, request: RenderRequest) {
    let book = catalog::active_book(request.book_id);
    fill_rect(fb, Rect::new(16, 454, 768, 2), false);
    draw_ascii(fb, book.title, 16, 462, false);

    let mut screen_buf = [0u8; 10];
    let mut total_buf = [0u8; 10];
    draw_ascii(
        fb,
        fmt_u32(request.page + 1, &mut screen_buf),
        376,
        462,
        false,
    );
    draw_ascii(fb, "/", 400, 462, false);
    draw_ascii(fb, fmt_u32(1, &mut total_buf), 416, 462, false);

    draw_battery_icon(fb, 728, 460, battery_bars(request.battery_percent));
    draw_progress_bar(
        fb,
        Rect::new(16, 448, 768, 3),
        book_progress_permille(request),
    );
}

fn draw_library_landscape(fb: &mut Framebuffer, request: RenderRequest) {
    draw_ascii(fb, "FILES", 96, 112, false);
    draw_ascii(fb, "/books/*.epub", 96, 144, false);
    let mut item_y = 204;
    for index in 0..catalog::book_count() as usize {
        let Some(book) = catalog::book_at(index) else {
            continue;
        };
        let selected = index == request.selection as usize;
        if selected {
            fill_rect(fb, Rect::new(88, item_y as u16 - 10, 624, 28), false);
        }
        draw_ascii(fb, if selected { ">" } else { " " }, 104, item_y, selected);
        draw_ascii(fb, book.title, 136, item_y, selected);
        item_y += 44;
    }
}

fn draw_chapters(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    draw_ascii(fb, "CHAPTERS", 96, 112, false);
    if request.book_id >= 2 {
        draw_sd_chapters(fb, request, sd_library);
        return;
    }
    let mut item_y = 168;
    for index in 0..catalog::chapter_count() as usize {
        let Some(chapter) = catalog::chapter_at(index) else {
            continue;
        };
        let selected = index == request.selection as usize;
        if selected {
            fill_rect(fb, Rect::new(88, item_y as u16 - 10, 624, 28), false);
        }
        draw_ascii(fb, if selected { ">" } else { " " }, 104, item_y, selected);
        draw_ascii(fb, chapter.title, 136, item_y, selected);
        item_y += 44;
    }
}

fn draw_sd_chapters(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    if sd_library.toc_count > 0 {
        draw_sd_toc_chapters(fb, request, sd_library);
        return;
    }
    let page_count = sd_library.page_count.max(1) as u32;
    let selected = request.selection as u32;
    let first = selected.saturating_sub(4);
    let mut item_y = 168usize;
    for page in first..first.saturating_add(8) {
        if page >= page_count.max(selected + 1) {
            break;
        }
        let is_selected = page == selected;
        if is_selected {
            fill_rect(fb, Rect::new(88, item_y as u16 - 10, 624, 28), false);
        }
        let mut page_buf = [0u8; 10];
        draw_ascii(
            fb,
            if is_selected { ">" } else { " " },
            104,
            item_y,
            is_selected,
        );
        draw_ascii(fb, "PAGE", 136, item_y, is_selected);
        draw_ascii(
            fb,
            fmt_u32(page + 1, &mut page_buf),
            184,
            item_y,
            is_selected,
        );
        item_y += 36;
    }
    draw_ascii(fb, "OK JUMPS TO PAGE", 96, 408, false);
}

fn draw_sd_toc_chapters(fb: &mut Framebuffer, request: RenderRequest, sd_library: &ReaderStore) {
    let chapter_count = sd_library.toc_count.max(1);
    let selected = (request.selection as usize).min(chapter_count.saturating_sub(1));
    let first = selected.saturating_sub(4);
    let mut item_y = 168usize;
    for index in first..first.saturating_add(8).min(chapter_count) {
        let is_selected = index == selected;
        if is_selected {
            fill_rect(fb, Rect::new(88, item_y as u16 - 10, 624, 28), false);
        }
        draw_ascii(
            fb,
            if is_selected { ">" } else { " " },
            104,
            item_y,
            is_selected,
        );
        let indent = 136 + (sd_library.toc[index].level.saturating_sub(1) as usize * 18);
        draw_ascii_truncated(
            fb,
            sd_library.toc_title(index),
            indent,
            item_y,
            66usize.saturating_sub(sd_library.toc[index].level as usize * 2),
            is_selected,
        );
        item_y += 36;
    }
    draw_ascii(fb, "OK JUMPS TO CHAPTER", 96, 408, false);
}

fn draw_menu(fb: &mut Framebuffer, title: &str, items: &[&str], selection: u8, y: usize) {
    draw_ascii(fb, title, 96, y, false);
    let mut item_y = y + 56;
    for (index, item) in items.iter().enumerate() {
        let selected = index == selection as usize;
        if selected {
            fill_rect(fb, Rect::new(88, item_y as u16 - 10, 624, 28), false);
        }
        draw_ascii(fb, if selected { ">" } else { " " }, 104, item_y, selected);
        draw_ascii(fb, item, 136, item_y, selected);
        item_y += 44;
    }
}

fn draw_ascii_truncated(
    fb: &mut Framebuffer,
    text: &str,
    x: usize,
    y: usize,
    max_chars: usize,
    inverted: bool,
) {
    let mut out = String::<96>::new();
    for ch in text.chars().take(max_chars) {
        if out.push(ch).is_err() {
            break;
        }
    }
    draw_ascii(fb, out.as_str(), x, y, inverted);
}

fn draw_settings(fb: &mut Framebuffer, request: RenderRequest) {
    draw_menu(fb, "SETTINGS", &SETTINGS_ITEMS, request.selection, 96);
    draw_ascii(fb, "CURRENT", 96, 292, false);
    draw_ascii(fb, orientation_label(request.orientation), 200, 292, false);
    draw_ascii(fb, "REFRESH", 96, 324, false);
    draw_ascii(
        fb,
        refresh_policy_label(request.refresh_policy),
        200,
        324,
        false,
    );
}

fn draw_progress_bar(fb: &mut Framebuffer, rect: Rect, permille: u16) {
    stroke_rect(fb, rect, false);
    let inner_w = rect.w.saturating_sub(4);
    let fill_w = ((inner_w as u32 * permille.min(1000) as u32) / 1000) as u16;
    if fill_w > 0 {
        let fill_h = rect.h.saturating_sub(4).max(1);
        let fill_y = if rect.h > 4 { rect.y + 2 } else { rect.y + 1 };
        fill_rect(fb, Rect::new(rect.x + 2, fill_y, fill_w, fill_h), false);
    }
}

fn centered_x_for(width: usize, text: &str) -> usize {
    width.saturating_sub(text.len() * 8) / 2
}

fn draw_battery_icon(fb: &mut Framebuffer, x: u16, y: u16, bars: u8) {
    stroke_rect(fb, Rect::new(x, y, 36, 16), false);
    fill_rect(fb, Rect::new(x + 36, y + 5, 4, 6), false);
    for bar in 0..bars.min(4) {
        fill_rect(fb, Rect::new(x + 4 + bar as u16 * 8, y + 4, 5, 8), false);
    }
}

fn battery_bars(percent: u8) -> u8 {
    match percent {
        0..=10 => 0,
        11..=35 => 1,
        36..=60 => 2,
        61..=85 => 3,
        _ => 4,
    }
}

fn book_progress_permille(request: RenderRequest) -> u16 {
    let chapters = catalog::chapter_count().max(1) as u32;
    ((request.chapter as u32 * 1000) / chapters.saturating_sub(1).max(1)) as u16
}

fn draw_rule(fb: &mut Framebuffer, y: usize) {
    fill_rect(fb, Rect::new(32, y as u16, 736, 2), false);
}

fn view_label(view: AppView) -> &'static str {
    match view {
        AppView::Home => "HOME",
        AppView::Library => "LIBRARY",
        AppView::Reading => "READING",
        AppView::Chapters => "CHAPTERS",
        AppView::Sync => "SYNC",
        AppView::Settings => "SETTINGS",
    }
}

fn orientation_label(orientation: DisplayOrientation) -> &'static str {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => "LANDSCAPE BOTTOM",
        DisplayOrientation::LandscapeButtonsTop => "LANDSCAPE TOP",
        DisplayOrientation::PortraitButtonsLeft => "PORTRAIT LEFT",
        DisplayOrientation::PortraitButtonsRight => "PORTRAIT RIGHT",
    }
}

fn refresh_policy_label(policy: RefreshPolicy) -> &'static str {
    match policy {
        RefreshPolicy::FastOnly => "FAST ONLY",
        RefreshPolicy::FullOnWake => "FULL ON WAKE",
        RefreshPolicy::FullEveryTen => "FULL EVERY 10",
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

fn mirror_framebuffer_long_axis(fb: &mut Framebuffer) {
    for y in 0..HEIGHT / 2 {
        let other_y = HEIGHT - 1 - y;
        for x in 0..WIDTH {
            let top = fb.pixel(x, y);
            let bottom = fb.pixel(x, other_y);
            fb.set_pixel(x, y, bottom);
            fb.set_pixel(x, other_y, top);
        }
    }
}

fn fmt_u32(n: u32, buf: &mut [u8; 10]) -> &str {
    let mut i = buf.len();
    let mut v = n;
    if v == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

fn fmt_percent(n: u8, buf: &mut [u8; 10]) -> &str {
    let mut tmp = [0u8; 10];
    let number = fmt_u32(n as u32, &mut tmp).as_bytes();
    if number.len() + 1 > buf.len() {
        return "?";
    }
    buf[..number.len()].copy_from_slice(number);
    buf[number.len()] = b'%';
    core::str::from_utf8(&buf[..number.len() + 1]).unwrap_or("?")
}
