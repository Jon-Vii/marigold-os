use crate::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiTocItem, UiView,
};
use app_core::{AppView, Button, DisplayOrientation, RefreshPolicy, RenderRequest};
use display::fb::Framebuffer;
use display::font::{draw_text, literata_display, literata_small, measure_text, FontStyle};
use display::render::draw_ascii;
use display::{HEIGHT, WIDTH};

#[derive(Clone, Copy, Debug)]
pub struct UiRenderModel<'a> {
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    pub library_entries: &'a [&'a str],
    pub chapters: &'a [UiTocItem<'a>],
}

pub fn render_request(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    if request.view == AppView::Reading {
        render_builtin_reading(fb, request, model);
        return;
    }

    let shell = UiShell {
        view: ui_view(request.view),
        orientation: ui_orientation(request.orientation),
        refresh_policy: ui_refresh_policy(request.refresh_policy),
        font_size: request.font_size,
        line_spacing: request.line_spacing,
        selection: request.selection,
        chapter: request.chapter,
        page: request.page,
        page_count: request.page_count,
        battery_percent: request.battery_percent,
        active_book: model.active_book,
        library_status: model.library_status,
        library_entries: model.library_entries,
        chapters: model.chapters,
    };
    render_shell(fb, &shell);
}

/// The sleep bookplate: no key is listening, so there is no margin
/// rail — the one ceremonial centered screen. Same furniture as home
/// (caps author, progress rule, italic chapter name), centered. No
/// battery; a days-old panel image must not show stale numbers.
pub fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.clear(true);
    let title_font = literata_display();
    let (first, second) = crate::render::wrap_title(title_font, model.active_book.title, 720);
    if second.is_empty() {
        draw_font_centered_fit(fb, title_font, first, 400, 204, 720);
    } else {
        // Two-line titles grow upward so the author/rule furniture
        // below keeps its place, mirroring the home title page.
        draw_font_centered_fit(fb, title_font, first, 400, 204 - 54, 720);
        draw_font_centered_fit(fb, title_font, second, 400, 204, 720);
    }
    if !model.active_book.author.is_empty() {
        let caps = literata_small(FontStyle::Regular);
        let width = crate::render::ls_width(caps, model.active_book.author, 3);
        crate::render::ls_caps(
            fb,
            caps,
            model.active_book.author,
            400 - width / 2,
            246,
            3,
        );
    }

    let permille = if request.page_count > 1 {
        (((request.page + 1).min(request.page_count) as u64 * 1000) / request.page_count as u64)
            as u16
    } else {
        model.active_book.progress_permille
    };
    crate::render::progress_rule(fb, 280, 302, 240, permille);

    let colophon_w = crate::render::chapter_colophon_width(model.chapters, request.chapter, 600);
    crate::render::draw_chapter_colophon(
        fb,
        model.chapters,
        request.chapter,
        400 - colophon_w / 2,
        340,
        600,
    );

    draw_font_centered_fit(
        fb,
        literata_small(FontStyle::Regular),
        "\u{00B7} asleep \u{00B7}",
        400,
        456,
        600,
    );
    mirror_framebuffer_long_axis(fb);
}

fn draw_font_centered_fit(
    fb: &mut Framebuffer,
    font: &display::font::BitmapFont,
    text: &str,
    cx: i16,
    y: i16,
    max_w: u16,
) {
    let mut shown = text;
    while measure_text(font, shown) > max_w && !shown.is_empty() {
        let mut end = shown.len() - 1;
        while end > 0 && !shown.is_char_boundary(end) {
            end -= 1;
        }
        shown = shown[..end].trim_end();
    }
    let x = cx - measure_text(font, shown) as i16 / 2;
    draw_text(fb, font, shown, x, y, false);
}

fn push_bytes(buf: &mut [u8], cursor: &mut usize, value: &str) {
    for byte in value.bytes() {
        if *cursor >= buf.len() {
            return;
        }
        buf[*cursor] = byte;
        *cursor += 1;
    }
}

fn push_number(buf: &mut [u8], cursor: &mut usize, value: usize) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    let mut value = value;
    if value == 0 {
        digits[0] = b'0';
        len = 1;
    }
    while value > 0 && len < digits.len() {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
    }
    for index in (0..len).rev() {
        if *cursor >= buf.len() {
            return;
        }
        buf[*cursor] = digits[index];
        *cursor += 1;
    }
}

fn render_builtin_reading(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.clear(true);
    draw_ascii(fb, "READ MODE", 64, 96, false);
    draw_ascii(fb, model.active_book.title, 64, 136, false);
    draw_ascii(fb, "BACK RETURNS HOME", 64, 176, false);
    let mut chapter_buf = [0u8; 10];
    draw_ascii(fb, "CHAPTER", 64, 232, false);
    draw_ascii(
        fb,
        fmt_u32(request.chapter as u32 + 1, &mut chapter_buf),
        160,
        232,
        false,
    );
    if let Some(button) = request.last_button {
        draw_ascii(fb, button_label(button), 64, 280, false);
    }
    mirror_framebuffer_long_axis(fb);
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

fn button_label(button: Button) -> &'static str {
    match button {
        Button::Power => "POWER",
        Button::Back => "BACK",
        Button::Confirm => "OK",
        Button::Previous => "PREV",
        Button::Next => "NEXT",
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

