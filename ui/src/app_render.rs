use crate::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiSyncStatus, UiTocItem, UiView,
};
use app_core::{
    AppView, Button, DisplayOrientation, RefreshPolicy, RenderRequest, SyncError, SyncStatus,
};
use display::fb::Framebuffer;
use display::font::{draw_text, literata_display, literata_small, measure_text, FontStyle};
use display::render::draw_ascii;
use display::{HEIGHT, WIDTH};

#[derive(Clone, Copy, Debug)]
pub struct UiRenderModel<'a> {
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    pub library_entries: &'a [&'a str],
    /// Absolute catalog index of `library_entries[0]`; the resident window
    /// the firmware streamed in around the current selection.
    pub library_window_start: u16,
    pub chapters: &'a [UiTocItem<'a>],
    /// Absolute TOC index of `chapters[0]` and the full on-disk chapter
    /// count; long TOCs stream a window around the visible rows.
    pub chapters_window_start: u16,
    pub chapters_total: u16,
    /// Current chapter title resolved over the whole book; empty for built-in
    /// books or before a book is open. See `UiShell::chapter_title`.
    pub chapter_title: &'a str,
    pub custom_font_name: &'a str,
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
        font_weight: request.font_weight,
        font_family: request.font_family,
        custom_font_name: model.custom_font_name,
        selection: request.selection,
        chapter: request.chapter,
        chapter_title: model.chapter_title,
        page: request.page,
        page_count: request.page_count,
        battery_percent: request.battery_percent,
        active_book: model.active_book,
        library_status: model.library_status,
        library_entries: model.library_entries,
        library_window_start: model.library_window_start,
        library_total: request.library_count,
        chapters: model.chapters,
        chapters_window_start: model.chapters_window_start,
        chapters_total: model.chapters_total,
        sync_status: ui_sync_status(request.sync_status),
        wifi_ssid: core::str::from_utf8(
            &request.wifi_ssid[..request.wifi_ssid_len.min(32) as usize],
        )
        .unwrap_or(""),
    };
    render_shell(fb, &shell);
}

fn ui_sync_status(status: SyncStatus) -> UiSyncStatus {
    match status {
        SyncStatus::NotConfigured => UiSyncStatus::NotConfigured,
        SyncStatus::Idle => UiSyncStatus::Idle,
        SyncStatus::ForgetPending => UiSyncStatus::ForgetPending,
        SyncStatus::Starting => UiSyncStatus::Starting,
        SyncStatus::Connecting => UiSyncStatus::Connecting,
        SyncStatus::Connected(ip) => UiSyncStatus::Connected(ip),
        SyncStatus::PortalUp => UiSyncStatus::PortalUp,
        SyncStatus::Serving(ip) => UiSyncStatus::Serving(ip),
        SyncStatus::CredentialsSaved => UiSyncStatus::CredentialsSaved,
        SyncStatus::Error(error) => UiSyncStatus::Error(sync_error_label(error)),
    }
}

fn sync_error_label(error: SyncError) -> &'static str {
    match error {
        SyncError::RadioInit => "radio failed",
        SyncError::Join => "wi-fi join failed",
        SyncError::Dhcp => "no network address",
    }
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
        crate::render::ls_caps(fb, caps, model.active_book.author, 400 - width / 2, 246, 3);
    }

    let permille = if request.page_count > 1 {
        (((request.page + 1).min(request.page_count) as u64 * 1000) / request.page_count as u64)
            as u16
    } else {
        model.active_book.progress_permille
    };
    crate::render::progress_rule(fb, 280, 302, 240, permille);

    // The sleep colophon is centered on the full 800px panel, so it can run
    // wider than Home's left-column colophon before a long chapter name needs
    // truncating.
    const SLEEP_COLOPHON_MAX_W: i16 = 720;
    let colophon_w = crate::render::chapter_colophon_width(
        model.chapters,
        request.chapter,
        model.chapter_title,
        SLEEP_COLOPHON_MAX_W,
    );
    crate::render::draw_chapter_colophon(
        fb,
        model.chapters,
        request.chapter,
        model.chapter_title,
        400 - colophon_w / 2,
        340,
        SLEEP_COLOPHON_MAX_W,
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
        AppView::Wireless => UiView::Wireless,
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
        Button::PagePrevious => "PAGE-",
        Button::PageNext => "PAGE+",
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
