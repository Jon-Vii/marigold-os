use crate::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiTocItem, UiView,
};
use app_core::{AppView, Button, DisplayOrientation, RefreshPolicy, RenderRequest};
use display::fb::Framebuffer;
use display::render::{draw_ascii, fill_rect, stroke_rect};
use display::{Rect, HEIGHT, WIDTH};

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
        selection: request.selection,
        battery_percent: request.battery_percent,
        active_book: model.active_book,
        library_status: model.library_status,
        library_entries: model.library_entries,
        chapters: model.chapters,
    };
    render_shell(fb, &shell);
}

pub fn render_sleep(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.clear(true);
    stroke_rect(fb, Rect::new(0, 0, WIDTH as u16, HEIGHT as u16), false);
    draw_ascii(fb, "SLEEPING", 360, 144, false);
    draw_ascii_centered(fb, model.active_book.title, 216);
    draw_ascii_centered(fb, model.active_book.author, 248);
    draw_progress_bar(
        fb,
        Rect::new(252, 286, 296, 6),
        model.active_book.progress_permille,
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

fn draw_ascii_centered(fb: &mut Framebuffer, text: &str, y: usize) {
    draw_ascii(fb, text, centered_x_for(WIDTH, text), y, false);
}

fn centered_x_for(width: usize, text: &str) -> usize {
    width.saturating_sub(text.len() * 8) / 2
}

fn draw_progress_bar(fb: &mut Framebuffer, rect: Rect, permille: u16) {
    stroke_rect(fb, rect, false);
    let inner_w = rect.w.saturating_sub(2);
    let fill_w = ((inner_w as u32 * permille.min(1000) as u32) / 1000) as u16;
    if fill_w > 0 {
        fill_rect(
            fb,
            Rect::new(rect.x + 1, rect.y + 1, fill_w, rect.h.saturating_sub(2)),
            false,
        );
    }
}

fn draw_battery_icon(fb: &mut Framebuffer, x: u16, y: u16, bars: u8) {
    stroke_rect(fb, Rect::new(x, y, 42, 18), false);
    fill_rect(fb, Rect::new(x + 42, y + 5, 4, 8), false);
    let bars = bars.min(4);
    for index in 0..bars {
        fill_rect(fb, Rect::new(x + 4 + index as u16 * 9, y + 4, 6, 10), false);
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
