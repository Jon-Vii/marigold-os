use crate::{
    render::render_shell, UiBook, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell,
    UiSyncStatus, UiTocItem, UiView,
};
use app_core::{
    AppView, DisplayOrientation, FirmwareUpdateStatus, FrontButtons, RefreshPolicy, RenderRequest,
    SyncError, SyncStatus,
};
use display::fb::{FbFrame, Framebuffer};
use display::font::{draw_text, literata_display, literata_small, measure_text, FontStyle};

#[derive(Clone, Copy, Debug)]
pub struct UiRenderModel<'a> {
    pub active_book: UiBook<'a>,
    pub library_status: UiLibraryStatus,
    pub library_entries: &'a [&'a str],
    /// Absolute catalog index of `library_entries[0]`; the resident window
    /// the firmware streamed in around the current selection.
    pub library_window_start: u16,
    pub firmware_entries: &'a [&'a str],
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

/// The drawing frame for an orientation: where `set_pixel` coordinates put
/// ink on the panel as held. Every renderer sets this before drawing; the
/// framebuffer is a long-lived static that remembers the previous frame.
pub fn fb_frame(orientation: DisplayOrientation) -> FbFrame {
    match orientation {
        DisplayOrientation::LandscapeButtonsBottom => FbFrame::Landscape,
        DisplayOrientation::LandscapeButtonsTop => FbFrame::LandscapeFlipped,
        DisplayOrientation::PortraitButtonsLeft | DisplayOrientation::PortraitButtonsRight => {
            FbFrame::Portrait
        }
    }
}

pub fn render_request(fb: &mut Framebuffer, request: RenderRequest, model: &UiRenderModel<'_>) {
    fb.set_frame(fb_frame(request.orientation));
    if request.view == AppView::Reading {
        // Book pages come from the firmware's reader store; the shared
        // renderer only ever holds the book's plate.
        fb.clear(true);
        crate::reading::draw_reading_bookplate(
            fb,
            model.active_book.title,
            model.active_book.author,
        );
        return;
    }

    let shell = UiShell {
        view: ui_view(request.view),
        orientation: ui_orientation(request.orientation),
        front_pages_left: request.front_buttons == FrontButtons::PagesLeft,
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
        firmware_status: ui_firmware_status(request.firmware_status),
        firmware_entries: model.firmware_entries,
        firmware_total: request.firmware_count,
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

fn ui_firmware_status(status: FirmwareUpdateStatus) -> crate::UiFirmwareStatus {
    match status {
        FirmwareUpdateStatus::Scanning => crate::UiFirmwareStatus::Scanning,
        FirmwareUpdateStatus::Ready => crate::UiFirmwareStatus::Ready,
        FirmwareUpdateStatus::Empty => crate::UiFirmwareStatus::Empty,
        FirmwareUpdateStatus::Confirming => crate::UiFirmwareStatus::Confirming,
        FirmwareUpdateStatus::Staging => crate::UiFirmwareStatus::Staging,
        FirmwareUpdateStatus::Failed => crate::UiFirmwareStatus::Failed,
    }
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
    fb.set_frame(fb_frame(request.orientation));
    fb.clear(true);

    // Centered ceremonial furniture, sized to the frame: the landscape
    // plate keeps its historical 800-wide geometry (including the X3's
    // 400-centered block); the portrait plate narrows to the upright leaf
    // and drops the block deeper into it.
    let portrait = fb.frame_width() < fb.frame_height();
    let cx = if portrait {
        fb.frame_width() as i16 / 2
    } else {
        400
    };
    let max_w = if portrait {
        fb.frame_width() as i16 - 60
    } else {
        720
    };
    let title_y = if portrait {
        fb.frame_height() as i16 * 3 / 7
    } else {
        204
    };

    let title_font = literata_display();
    let (first, second) =
        crate::render::wrap_title(title_font, model.active_book.title, max_w as u16);
    if second.is_empty() {
        draw_font_centered_fit(fb, title_font, first, cx, title_y, max_w as u16);
    } else {
        // Two-line titles grow upward so the author/rule furniture
        // below keeps its place, mirroring the home title page.
        draw_font_centered_fit(fb, title_font, first, cx, title_y - 54, max_w as u16);
        draw_font_centered_fit(fb, title_font, second, cx, title_y, max_w as u16);
    }
    if !model.active_book.author.is_empty() {
        let caps = literata_small(FontStyle::Regular);
        let width = crate::render::ls_width(caps, model.active_book.author, 3);
        crate::render::ls_caps(
            fb,
            caps,
            model.active_book.author,
            cx - width / 2,
            title_y + 42,
            3,
        );
    }

    let permille = if request.page_count > 1 {
        (((request.page + 1).min(request.page_count) as u64 * 1000) / request.page_count as u64)
            as u16
    } else {
        model.active_book.progress_permille
    };
    crate::render::progress_rule(fb, cx - 120, title_y + 98, 240, permille);

    // The sleep colophon is centered on the full frame width, so it can run
    // wider than Home's left-column colophon before a long chapter name
    // needs truncating.
    let colophon_max_w = max_w;
    let colophon_w = crate::render::chapter_colophon_width(
        model.chapters,
        request.chapter,
        model.chapter_title,
        colophon_max_w,
    );
    crate::render::draw_chapter_colophon(
        fb,
        model.chapters,
        request.chapter,
        model.chapter_title,
        cx - colophon_w / 2,
        title_y + 136,
        colophon_max_w,
    );
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

fn ui_view(view: AppView) -> UiView {
    match view {
        AppView::Home => UiView::Home,
        AppView::Library => UiView::Library,
        AppView::Reading => UiView::Home,
        AppView::Chapters => UiView::Chapters,
        AppView::Wireless => UiView::Wireless,
        AppView::Settings => UiView::Settings,
        AppView::FirmwareUpdate => UiView::FirmwareUpdate,
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
