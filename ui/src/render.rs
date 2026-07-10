//! The Imprint shell: the UI is typeset like a fine book. Three text
//! voices — upright body for content, italic for the book's voice,
//! letterspaced small caps for the device's voice (headings and the
//! margin keys). The four left-bezel buttons get em-dash margin notes
//! aligned beside them (KEY_YS); key order is semantic: slot 1 is the
//! screen's primary action (bold caps), slot 2 the way elsewhere/out,
//! slots 3-4 the paired browse keys. Apparatus shows battery percent
//! only — the device does not tell time.

use crate::{
    qr_generated, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell, UiSyncStatus,
    UiTocItem, UiView,
};
use display::fb::{FbFrame, Framebuffer};
use display::font::{
    draw_text, literata, literata_display, literata_small, measure_text, BitmapFont, FontStyle,
};
use display::render::fill_rect;
use display::{Rect, HEIGHT, WIDTH};

/// Vertical centers of the four left-bezel buttons on screen,
/// top to bottom: Back, Confirm, Previous, Next. Positioned as a fraction
/// of panel height (the X4's 480 gives the historical 120/200/280/360), so
/// the same button arrangement stays aligned on the X3's differently sized
/// panel rather than bunching toward the top.
const KEY_YS: [i16; 4] = [key_y(120), key_y(200), key_y(280), key_y(360)];

const fn key_y(x4_y: i16) -> i16 {
    (x4_y as i32 * HEIGHT as i32 / 480) as i16
}
const KEY_DASH_X: i16 = 10;
const KEY_LABEL_X: i16 = 40;
const CONTENT_X: i16 = 210;
const CONTENT_RIGHT: i16 = 740;
/// The chapter colophon is a single line with nothing to its right on that row,
/// so it may run past the wrapped-title content column out to the panel margin
/// (matching the centered sleep colophon's edge) before a long name truncates.
const COLOPHON_RIGHT: i16 = 760;
const HEADING_CX: i16 = 480;
/// Right edge for the footer battery readout. It sits in the panel corner
/// with nothing beside it, so it tucks to a 24px inset from the panel edge
/// rather than the content column's value margin — the corner then reads the
/// same in a menu as when reading. Panel-relative (the X4's historical 776).
const FOOTER_RIGHT: i16 = WIDTH as i16 - 24;
const ROW_STEP: i16 = 56;
const FIRST_ROW_Y: i16 = 118;
/// Rows the Library list shows at once. Public so the firmware slides the
/// resident catalog window over the visible range it must stream in. The
/// portrait page runs the long axis upright, so it seats more rows above
/// its bottom key rail than landscape does above its footer line.
pub const fn library_visible_rows(portrait: bool) -> usize {
    if portrait {
        10
    } else {
        6
    }
}
/// Footer baseline: 24px up from the panel's bottom edge (the X4's
/// historical 456). Panel-relative so the taller X3 keeps its apparatus in
/// the corner rather than floating it mid-page.
const FOOTER_Y: i16 = HEIGHT as i16 - 24;
/// Baseline-to-baseline leading for the wrapped 46px display title,
/// tighter than the face's default 62px line height as title blocks
/// conventionally are.
const TITLE_LEADING: i16 = 54;

#[derive(Clone, Copy)]
struct ShellLayout {
    mirrored: bool,
    portrait: bool,
    /// Front buttons run pages-first: the key rail's slot labels must sit
    /// beside the buttons that now carry them, so slot lookups rotate the
    /// two pairs.
    pages_left: bool,
    /// Drawing-frame height: the panel's long axis stands upright in
    /// portrait, so vertical furniture (footer, key rail) hangs off this
    /// rather than the panel HEIGHT constant.
    frame_height: i16,
    content_x: i16,
    content_right: i16,
    colophon_right: i16,
    heading_cx: i16,
}

impl ShellLayout {
    const fn for_orientation(orientation: UiOrientation) -> Self {
        match orientation {
            UiOrientation::LandscapeButtonsTop => Self {
                mirrored: true,
                portrait: false,
                pages_left: false,
                frame_height: HEIGHT as i16,
                content_x: WIDTH as i16 - CONTENT_RIGHT,
                content_right: WIDTH as i16 - CONTENT_X,
                colophon_right: WIDTH as i16 - (COLOPHON_RIGHT - CONTENT_RIGHT),
                heading_cx: WIDTH as i16 - HEADING_CX,
            },
            UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => {
                // The margin rail moves to the bottom edge beside the front
                // buttons, so content spans the frame's full (short) width.
                let width = FbFrame::Portrait.width() as i16;
                Self {
                    mirrored: false,
                    portrait: true,
                    pages_left: false,
                    frame_height: FbFrame::Portrait.height() as i16,
                    content_x: 44,
                    content_right: width - 36,
                    colophon_right: width - 24,
                    heading_cx: width / 2,
                }
            }
            _ => Self {
                mirrored: false,
                portrait: false,
                pages_left: false,
                frame_height: HEIGHT as i16,
                content_x: CONTENT_X,
                content_right: CONTENT_RIGHT,
                colophon_right: COLOPHON_RIGHT,
                heading_cx: HEADING_CX,
            },
        }
    }

    /// Physical key position for a semantic slot. With pages-left front
    /// buttons the two pairs trade places whole, so semantic slots 0-1
    /// (back, primary) draw beside physical positions 2-3 and vice versa.
    const fn key_pos(self, slot: usize) -> i16 {
        if self.pages_left {
            KEY_YS[(slot + 2) % 4]
        } else {
            KEY_YS[slot]
        }
    }

    const fn content_width(self) -> i16 {
        self.content_right - self.content_x
    }

    const fn selection_x(self) -> i16 {
        if self.mirrored {
            self.content_right + 22
        } else {
            self.content_x - 32
        }
    }

    /// Footer baseline: the panel-bottom corner line in landscape; lifted
    /// above the bottom key rail in portrait.
    const fn footer_y(self) -> i16 {
        if self.portrait {
            self.frame_height - 100
        } else {
            FOOTER_Y
        }
    }
}

fn shell_layout(shell: &UiShell<'_>) -> ShellLayout {
    let mut layout = ShellLayout::for_orientation(shell.orientation);
    layout.pages_left = shell.front_pages_left;
    layout
}

pub fn render_shell(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters(fb, shell),
        UiView::Wireless => render_wireless(fb, shell),
        UiView::Settings => render_settings(fb, shell),
    }
}

pub fn render_shell_overlay(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    render_shell(fb, shell);
}

/// Home is a title page: big title, the author in letterspaced caps,
/// the progress rule, and a colophon in chapter-and-pages terms.
fn render_home(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let mut layout = shell_layout(shell);
    // Home direct-maps the physical key column and the reducer ignores the
    // front-pair swap there (see apply_input), so its labels stay put too.
    layout.pages_left = false;
    dash_key(fb, layout, 0, "library", false);
    dash_key(fb, layout, 1, "continue", true);
    dash_key(fb, layout, 2, "wireless", false);
    dash_key(fb, layout, 3, "settings", false);

    // Long titles wrap to further lines that grow upward, keeping the
    // author/rule/colophon furniture (and one-line titles) fixed. The
    // portrait title page drops its block deeper into the taller leaf
    // (the same three-eighths position), keeping the block's own leading —
    // and its narrower measure gets a third line, since titles that fill
    // two landscape lines need three upright ones.
    let title_y = if layout.portrait {
        layout.frame_height * 3 / 8
    } else {
        180
    };
    let max_lines = if layout.portrait { 3 } else { 2 };
    let title_font = literata_display();
    let (lines, count, overflow) = wrap_title_lines(
        title_font,
        shell.active_book.title,
        layout.content_width() as u16,
        max_lines,
    );
    for (index, line) in lines[..count].iter().enumerate() {
        let y = title_y - TITLE_LEADING * (count - 1 - index) as i16;
        let end = draw_text(fb, title_font, line, layout.content_x, y, false);
        if overflow && index == count - 1 {
            draw_text(fb, title_font, "...", end, y, false);
        }
    }
    if !shell.active_book.author.is_empty() {
        ls_caps(
            fb,
            literata_small(FontStyle::Regular),
            shell.active_book.author,
            layout.content_x,
            title_y + 42,
            3,
        );
    }

    let permille = if shell.page_count > 1 {
        (((shell.page + 1).min(shell.page_count) as u64 * 1000) / shell.page_count as u64) as u16
    } else {
        shell.active_book.progress_permille
    };
    progress_rule(fb, layout.content_x, title_y + 100, 240, permille);

    // Colophon: the chapter name alone, in the book's italic voice —
    // the progress rule already answers "how far". Roman numeral
    // fallback when the book has no usable chapter title.
    draw_chapter_colophon(
        fb,
        shell.chapters,
        shell.chapter,
        shell.chapter_title,
        layout.content_x,
        title_y + 132,
        layout.colophon_right - layout.content_x,
    );

    draw_battery_percent(fb, layout, shell.battery_percent);
}

pub(crate) fn draw_chapter_colophon(
    fb: &mut Framebuffer,
    chapters: &[UiTocItem<'_>],
    chapter: u8,
    title_override: &str,
    x: i16,
    baseline: i16,
    max_w: i16,
) -> i16 {
    let chapter_name = colophon_chapter_name(chapters, chapter, title_override);
    if chapter_name.is_empty() {
        let mut numeral = [0u8; 16];
        let mut cursor = 0;
        push_roman(&mut numeral, &mut cursor, chapter as usize + 1);
        let numeral = core::str::from_utf8(&numeral[..cursor]).unwrap_or("");
        draw_text(
            fb,
            literata_small(FontStyle::Regular),
            numeral,
            x,
            baseline,
            false,
        )
    } else {
        let italic = literata_small(FontStyle::Italic);
        let shown = fit_text(italic, chapter_name, max_w.max(60) as u16);
        draw_text(fb, italic, shown, x, baseline, false)
    }
}

/// The chapter name the colophon shows: the firmware-resolved title when
/// present (covers the whole book), else the resident list entry.
fn colophon_chapter_name<'a>(
    chapters: &'a [UiTocItem<'a>],
    chapter: u8,
    title_override: &'a str,
) -> &'a str {
    if !title_override.is_empty() {
        return title_override;
    }
    chapters
        .get(chapter as usize)
        .map(|item| item.title)
        .unwrap_or("")
}

/// Width the colophon will occupy, for centered layouts.
pub(crate) fn chapter_colophon_width(
    chapters: &[UiTocItem<'_>],
    chapter: u8,
    title_override: &str,
    max_w: i16,
) -> i16 {
    let chapter_name = colophon_chapter_name(chapters, chapter, title_override);
    if chapter_name.is_empty() {
        let mut numeral = [0u8; 16];
        let mut cursor = 0;
        push_roman(&mut numeral, &mut cursor, chapter as usize + 1);
        let numeral = core::str::from_utf8(&numeral[..cursor]).unwrap_or("");
        measure_text(literata_small(FontStyle::Regular), numeral) as i16
    } else {
        let italic = literata_small(FontStyle::Italic);
        let shown = fit_text(italic, chapter_name, max_w.max(60) as u16);
        measure_text(italic, shown) as i16
    }
}

/// The 1px rule with a 3px head filled to the reading position.
pub(crate) fn progress_rule(fb: &mut Framebuffer, x: i16, y: i16, w: i16, permille: u16) {
    hline(fb, x, y, w);
    let fill = ((w as i32 * permille.min(1000) as i32) / 1000) as i16;
    fill_rect(
        fb,
        Rect::new(x as u16, (y - 1) as u16, fill.max(2) as u16, 3),
        false,
    );
}

fn push_roman(buf: &mut [u8], cursor: &mut usize, value: usize) {
    const PAIRS: [(usize, &str); 9] = [
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut remaining = value.min(399);
    for (weight, glyphs) in PAIRS {
        while remaining >= weight {
            push_str(buf, cursor, glyphs);
            remaining -= weight;
        }
    }
}

fn render_library(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "open", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Library");

    match shell.library_status {
        UiLibraryStatus::NotScanned | UiLibraryStatus::Scanning => {
            centered_note(fb, layout, "reading the card\u{2026}");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Error => {
            centered_note(fb, layout, "the library is unavailable");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Empty => {
            centered_note(fb, layout, "no books \u{2014} add EPUB files to /books");
            finish_working_screen(fb, shell, layout);
            return;
        }
        UiLibraryStatus::Ready => {}
    }
    let total = shell.library_total as usize;
    if total == 0 || shell.library_entries.is_empty() {
        centered_note(fb, layout, "no books \u{2014} add EPUB files to /books");
        finish_working_screen(fb, shell, layout);
        return;
    }

    // `selection` and `start` are absolute catalog indices; rows are read out
    // of the resident window, which the firmware guarantees covers the visible
    // range. A miss (stale window mid-refill) leaves that row blank rather than
    // drawing the wrong book.
    let selected_index = (shell.selection as usize).min(total.saturating_sub(1));
    let start = library_scroll_start(selected_index, total, layout.portrait);
    let window_start = shell.library_window_start as usize;
    let body = literata(FontStyle::Regular);
    let mut y = FIRST_ROW_Y;
    for row in 0..library_visible_rows(layout.portrait) {
        let abs = start + row;
        if abs >= total {
            break;
        }
        let Some(entry) = abs
            .checked_sub(window_start)
            .and_then(|offset| shell.library_entries.get(offset))
        else {
            y += ROW_STEP;
            continue;
        };
        if abs == selected_index {
            selection_arrow(fb, layout, y);
        }
        draw_text_truncated(
            fb,
            body,
            entry,
            layout.content_x,
            y,
            layout.content_width() as usize,
            false,
        );
        y += ROW_STEP;
    }

    position_footer(fb, layout, selected_index + 1, total);
    finish_working_screen(fb, shell, layout);
}

/// Absolute catalog index of the first visible Library row that keeps
/// `selection` on screen. Shared by the renderer and the firmware's window
/// loader so both agree on which slice of the catalog is resident.
pub fn library_scroll_start(selection: usize, total: usize, portrait: bool) -> usize {
    let rows = library_visible_rows(portrait);
    let start = if selection >= rows {
        selection + 1 - rows
    } else {
        0
    };
    start.min(total.saturating_sub(rows))
}

// The contents page uses tight index rows — a real table of contents,
// not a menu: title, dot leaders, the chapter's book page right-aligned.
const TOC_ROW_STEP: i16 = 36;

pub const fn toc_visible_rows(portrait: bool) -> usize {
    if portrait {
        15
    } else {
        9
    }
}

/// Absolute TOC index of the first visible Contents row that keeps
/// `selection` on screen. Shared by the renderer and the firmware's TOC
/// window loader so both agree on which slice must be resident.
pub fn toc_scroll_start(selection: usize, total: usize, portrait: bool) -> usize {
    let rows = toc_visible_rows(portrait);
    let start = if selection >= rows {
        selection + 1 - rows
    } else {
        0
    };
    start.min(total.saturating_sub(rows))
}

fn render_chapters(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    dash_key(fb, layout, 0, "close", false);
    dash_key(fb, layout, 1, "open", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Contents");

    let total = shell.chapters_total as usize;
    if total == 0 || shell.chapters.is_empty() {
        centered_note(fb, layout, "no chapters found");
        finish_working_screen(fb, shell, layout);
        return;
    }

    // The full chapter list streams off the card (~a second); until it lands
    // the resident list can be shorter than the reading position. A cursor
    // past its end must not snap back to the last resident row -- that paints
    // a wrong chapter that "jumps" forward on the first key. Hold a note until
    // the real list arrives and the selection is in range.
    if shell.selection as usize >= total {
        centered_note(fb, layout, "loading contents\u{2026}");
        finish_working_screen(fb, shell, layout);
        return;
    }

    // `selected` and `start` are absolute TOC indices; rows are read out of
    // the resident window, which the firmware slides over the visible range
    // before each render. A miss (stale window mid-refill) leaves that row
    // blank rather than drawing the wrong chapter.
    let selected = (shell.selection as usize).min(total - 1);
    let start = toc_scroll_start(selected, total, layout.portrait);
    let window_start = shell.chapters_window_start as usize;
    let mut y = FIRST_ROW_Y;
    for row in 0..toc_visible_rows(layout.portrait) {
        let abs = start + row;
        if abs >= total {
            break;
        }
        let Some(item) = abs
            .checked_sub(window_start)
            .and_then(|offset| shell.chapters.get(offset))
        else {
            y += TOC_ROW_STEP;
            continue;
        };
        if abs == selected {
            selection_arrow(fb, layout, y);
        }
        draw_toc_row(fb, layout, item, abs, y);
        y += TOC_ROW_STEP;
    }

    position_footer(fb, layout, selected + 1, total);
    finish_working_screen(fb, shell, layout);
}

fn render_settings(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "change", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Settings");

    // Seven rows must clear the landscape footer line, so the settings
    // index runs tighter than the Library's ROW_STEP.
    const SETTINGS_ROW_STEP: i16 = 52;
    let rows: [(&str, &str); 7] = [
        (
            "Typeface",
            font_family_label(shell.font_family, shell.custom_font_name),
        ),
        ("Type size", font_size_label(shell.font_size)),
        ("Type weight", font_weight_label(shell.font_weight)),
        ("Line spacing", line_spacing_label(shell.line_spacing)),
        ("Refresh", refresh_policy_label(shell.refresh_policy)),
        ("Orientation", orientation_label(shell.orientation)),
        ("Front buttons", front_buttons_label(shell.front_pages_left)),
    ];
    for (index, (name, value)) in rows.into_iter().enumerate() {
        index_row(
            fb,
            layout,
            name,
            value,
            FIRST_ROW_Y + index as i16 * SETTINGS_ROW_STEP,
            shell.selection == index as u16,
        );
    }

    finish_working_screen(fb, shell, layout);
}

fn render_wireless(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = shell_layout(shell);
    match shell.sync_status {
        UiSyncStatus::ForgetPending => dash_key(fb, layout, 0, "cancel", false),
        _ => dash_key(fb, layout, 0, "home", false),
    }
    match shell.sync_status {
        UiSyncStatus::Idle => dash_key(fb, layout, 1, "connect", true),
        UiSyncStatus::NotConfigured => dash_key(fb, layout, 1, "set up", true),
        UiSyncStatus::ForgetPending => dash_key(fb, layout, 1, "forget", true),
        UiSyncStatus::Error(_) => dash_key(fb, layout, 1, "again", true),
        UiSyncStatus::CredentialsSaved | UiSyncStatus::Serving(_) => {
            dash_key(fb, layout, 1, "done", true)
        }
        _ => dash_unused(fb, layout, 1),
    }
    match shell.sync_status {
        UiSyncStatus::Idle => dash_key(fb, layout, 2, "forget", false),
        _ => dash_unused(fb, layout, 2),
    }
    dash_unused(fb, layout, 3);
    heading(fb, layout, "Wireless");

    let hint_y = if layout.portrait { 330 } else { 280 };
    match shell.sync_status {
        UiSyncStatus::NotConfigured => {
            centered_note(fb, layout, "no wi-fi network saved yet");
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "set up opens a hotspot your phone can configure",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Idle => {
            let mut buf = [0u8; 48];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "network \u{201C}");
            push_str(&mut buf, &mut cursor, shell.wifi_ssid);
            push_str(&mut buf, &mut cursor, "\u{201D}");
            centered_note(fb, layout, text_in(&buf, cursor));
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "connect to add and manage books from your browser",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::ForgetPending => {
            let mut buf = [0u8; 48];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "forget \u{201C}");
            push_str(&mut buf, &mut cursor, shell.wifi_ssid);
            push_str(&mut buf, &mut cursor, "\u{201D}?");
            centered_note(fb, layout, text_in(&buf, cursor));
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "removes the saved wi-fi network \u{00b7} set up runs again next time",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Starting | UiSyncStatus::Connecting => {
            centered_note(fb, layout, "joining wi-fi \u{2026}");
        }
        UiSyncStatus::Connected(ip) => {
            let mut buf = [0u8; 40];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "connected at ");
            push_ipv4(&mut buf, &mut cursor, ip);
            centered_note(fb, layout, text_in(&buf, cursor));
        }
        UiSyncStatus::PortalUp => {
            // The QR joins the open hotspot; the captive DNS then raises
            // the phone's sign-in sheet with the credential form.
            draw_qr(
                fb,
                &qr_generated::QR_JOIN_BITS,
                qr_generated::QR_JOIN_SIZE,
                qr_generated::QR_JOIN_STRIDE,
                layout.heading_cx,
                160,
                5,
            );
            draw_text_centered(
                fb,
                literata_small(FontStyle::Regular),
                "scan to join \u{201c}XTEINK-X4\u{201d}",
                layout.heading_cx,
                348,
            );
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "then enter your wi-fi in the page that opens \u{00b7} http://192.168.4.1",
                layout.heading_cx,
                382,
            );
        }
        UiSyncStatus::Serving(ip) => {
            let mut buf = [0u8; 56];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "visit ");
            push_ipv4(&mut buf, &mut cursor, ip);
            push_str(&mut buf, &mut cursor, " to add and remove books");
            centered_note(fb, layout, text_in(&buf, cursor));
        }
        UiSyncStatus::CredentialsSaved => {
            centered_note(fb, layout, "wi-fi saved");
            draw_text_centered(
                fb,
                literata_small(FontStyle::Italic),
                "press done to restart, then connect from this screen",
                layout.heading_cx,
                hint_y,
            );
        }
        UiSyncStatus::Error(reason) => {
            let mut buf = [0u8; 64];
            let mut cursor = 0;
            push_str(&mut buf, &mut cursor, "could not connect \u{00B7} ");
            push_str(&mut buf, &mut cursor, reason);
            centered_note(fb, layout, text_in(&buf, cursor));
        }
    }

    finish_working_screen(fb, shell, layout);
}

/// Blits a packed QR matrix centered on `cx`, `scale` pixels per module,
/// with the quiet zone cleared around it.
fn draw_qr(
    fb: &mut Framebuffer,
    bits: &[u8],
    size: usize,
    stride: usize,
    cx: i16,
    top: i16,
    scale: i16,
) {
    let edge = size as i16 * scale;
    let left = (cx - edge / 2).max(0) as u16;
    let top = top.max(0) as u16;
    let quiet = (scale * 2) as u16;
    fill_rect(
        fb,
        Rect {
            x: left.saturating_sub(quiet),
            y: top.saturating_sub(quiet),
            w: edge as u16 + quiet * 2,
            h: edge as u16 + quiet * 2,
        },
        true,
    );
    let scale = scale as u16;
    for row in 0..size {
        for col in 0..size {
            if bits[row * stride + col / 8] & (0x80 >> (col % 8)) != 0 {
                fill_rect(
                    fb,
                    Rect {
                        x: left + col as u16 * scale,
                        y: top + row as u16 * scale,
                        w: scale,
                        h: scale,
                    },
                    false,
                );
            }
        }
    }
}

fn push_ipv4(buf: &mut [u8], cursor: &mut usize, ip: [u8; 4]) {
    for (index, octet) in ip.iter().enumerate() {
        if index > 0 {
            push_str(buf, cursor, ".");
        }
        push_usize(buf, cursor, *octet as usize);
    }
}

fn text_in(buf: &[u8], len: usize) -> &str {
    core::str::from_utf8(&buf[..len]).unwrap_or("?")
}

// ------------------------------------------------------------------
// Imprint furniture
// ------------------------------------------------------------------

/// An em-dash faces the physical button; the label is letterspaced
/// small caps, bold for the screen's one primary action.
fn dash_key(fb: &mut Framebuffer, layout: ShellLayout, slot: usize, label: &str, primary: bool) {
    if layout.portrait {
        // Portrait keys are named by a 1bpp icon centered over the button,
        // not by a letterspaced label; `primary` carries no weight here.
        let glyph = crate::icons::icon_for_label(label);
        crate::icons::draw_icon(
            fb,
            glyph,
            layout.key_pos(slot) - crate::icons::ICON_SIZE / 2,
            layout.frame_height - 24 - crate::icons::ICON_SIZE / 2,
        );
        return;
    }
    let style = if primary {
        FontStyle::Bold
    } else {
        FontStyle::Regular
    };
    let label_font = literata_small(style);
    let y = layout.key_pos(slot);
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, y + 8, false);
    if layout.mirrored {
        let width = ls_width(label_font, label, 2);
        ls_caps(fb, label_font, label, dash_x - 24 - width, y + 6, 2);
    } else {
        ls_caps(fb, label_font, label, KEY_LABEL_X, y + 6, 2);
    }
}

/// An unused key keeps its bare dash: the mark stays, the word goes.
/// Portrait names keys with icons only, so an unused key shows nothing.
fn dash_unused(fb: &mut Framebuffer, layout: ShellLayout, slot: usize) {
    if layout.portrait {
        return;
    }
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, layout.key_pos(slot) + 8, false);
}

/// The summoned reading key sheet: portrait reading stays chrome-free, and
/// a named-key press raises this white band over the page's bottom edge —
/// footer and all, the page reserves nothing for it. A hairline closes its
/// top edge; inside, four 1bpp icons sit on one baseline, each centered
/// over the physical button it names (following the front-pair swap), so
/// the band stays shallow. Paging dismisses it (the reducer owns that).
pub fn render_reading_sheet(fb: &mut Framebuffer, orientation: UiOrientation, pages_left: bool) {
    let mut layout = ShellLayout::for_orientation(orientation);
    layout.pages_left = pages_left;
    // The reducer only holds the sheet up in portrait; landscape callers
    // never reach here.
    debug_assert!(layout.portrait);
    let top = layout.frame_height - crate::reading::PORTRAIT_READING_SHEET_HEIGHT;
    let width = fb.frame_width() as u16;
    fill_rect(
        fb,
        Rect::new(
            0,
            top as u16,
            width,
            crate::reading::PORTRAIT_READING_SHEET_HEIGHT as u16,
        ),
        true,
    );
    hline(fb, 0, top, width as i16);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "contents", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
}

fn heading(fb: &mut Framebuffer, layout: ShellLayout, text: &str) {
    let small = literata_small(FontStyle::Regular);
    let width = ls_width(small, text, 5);
    ls_caps(fb, small, text, layout.heading_cx - width / 2, 42, 5);
    hline(fb, layout.heading_cx - 160, 56, 320);
}

/// Letterspaced all-caps, the small-caps stand-in for this bitmap set.
pub(crate) fn ls_caps(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    baseline: i16,
    extra: i16,
) {
    let mut cursor = x;
    for ch in text.chars() {
        let upper = ch.to_ascii_uppercase();
        let mut buf = [0u8; 4];
        let glyph = upper.encode_utf8(&mut buf);
        cursor = draw_text(fb, font, glyph, cursor, baseline, false) + extra;
    }
}

pub(crate) fn ls_width(font: &BitmapFont, text: &str, extra: i16) -> i16 {
    let mut width = 0i16;
    let mut count = 0i16;
    for ch in text.chars() {
        let upper = ch.to_ascii_uppercase();
        let mut buf = [0u8; 4];
        let glyph = upper.encode_utf8(&mut buf);
        width += measure_text(font, glyph) as i16;
        count += 1;
    }
    width + (count - 1).max(0) * extra
}

/// Name, dot leaders, italic value right-aligned: the index pattern
/// shared by every list screen.
fn index_row(
    fb: &mut Framebuffer,
    layout: ShellLayout,
    name: &str,
    value: &str,
    y: i16,
    selected: bool,
) {
    if selected {
        selection_arrow(fb, layout, y);
    }
    let body = literata(FontStyle::Regular);
    let end_x = draw_text(fb, body, name, layout.content_x, y, false);
    let value_font = literata(FontStyle::Italic);
    let value_w = measure_text(value_font, value) as i16;
    let mut dx = end_x + 16;
    while dx < layout.content_right - value_w - 14 {
        fill_rect(fb, Rect::new(dx as u16, (y - 2) as u16, 1, 1), false);
        dx += 8;
    }
    draw_text(
        fb,
        value_font,
        value,
        layout.content_right - value_w,
        y,
        false,
    );
}

fn draw_toc_row(
    fb: &mut Framebuffer,
    layout: ShellLayout,
    item: &UiTocItem<'_>,
    index: usize,
    y: i16,
) {
    let body = literata(FontStyle::Regular);
    let indent = layout.content_x + (item.level.saturating_sub(1) as i16 * 18);
    let mut numbered = [0u8; 32];
    let title = if item.title.is_empty() {
        fmt_numbered_chapter(index + 1, &mut numbered)
    } else {
        item.title
    };

    if item.page == 0 {
        draw_text_truncated(
            fb,
            body,
            title,
            indent,
            y,
            (layout.content_right - indent).max(0) as usize,
            false,
        );
        return;
    }

    let mut page_buf = [0u8; 16];
    let mut cursor = 0;
    push_usize(&mut page_buf, &mut cursor, item.page as usize);
    let page = core::str::from_utf8(&page_buf[..cursor]).unwrap_or("");
    let page_w = measure_text(body, page) as i16;

    let title_max = (layout.content_right - indent - page_w - 40).max(40) as usize;
    let shown = fit_text(body, title, title_max as u16);
    let end_x = draw_text(fb, body, shown, indent, y, false);
    let mut dx = end_x + 16;
    while dx < layout.content_right - page_w - 14 {
        fill_rect(fb, Rect::new(dx as u16, (y - 2) as u16, 1, 1), false);
        dx += 8;
    }
    draw_text(fb, body, page, layout.content_right - page_w, y, false);
}

fn selection_arrow(fb: &mut Framebuffer, layout: ShellLayout, y: i16) {
    let arrow = if layout.mirrored {
        "\u{2190}"
    } else {
        "\u{2192}"
    };
    draw_text(
        fb,
        literata(FontStyle::Regular),
        arrow,
        layout.selection_x(),
        y,
        false,
    );
}

fn centered_note(fb: &mut Framebuffer, layout: ShellLayout, text: &str) {
    // The taller portrait page carries its note a little further down to
    // keep it in the same optical position under the heading rule.
    let y = if layout.portrait { 280 } else { 230 };
    draw_text_centered(fb, literata(FontStyle::Italic), text, layout.heading_cx, y);
}

/// "– n of m –" centered on the content column.
fn position_footer(fb: &mut Framebuffer, layout: ShellLayout, current: usize, total: usize) {
    let mut buf = [0u8; 32];
    let mut cursor = 0;
    push_str(&mut buf, &mut cursor, "\u{2013} ");
    push_usize(&mut buf, &mut cursor, current);
    push_str(&mut buf, &mut cursor, " of ");
    push_usize(&mut buf, &mut cursor, total);
    push_str(&mut buf, &mut cursor, " \u{2013}");
    let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
    draw_text_centered(
        fb,
        literata_small(FontStyle::Regular),
        label,
        layout.heading_cx,
        layout.footer_y(),
    );
}

fn draw_battery_percent(fb: &mut Framebuffer, layout: ShellLayout, percent: u8) {
    let mut buf = [0u8; 8];
    let mut cursor = 0;
    push_usize(&mut buf, &mut cursor, percent.min(100) as usize);
    push_str(&mut buf, &mut cursor, "%");
    let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
    let small = literata_small(FontStyle::Regular);
    if layout.portrait {
        // The battery percentage sits tucked in the top-right corner,
        // above the heading area to avoid overlapping.
        let width = measure_text(small, label) as i16;
        let frame_width = fb.frame_width() as i16;
        draw_text(fb, small, label, frame_width - 16 - width, 24, false);
    } else if layout.mirrored {
        draw_text(
            fb,
            small,
            label,
            WIDTH as i16 - FOOTER_RIGHT,
            FOOTER_Y,
            false,
        );
    } else {
        let width = measure_text(small, label) as i16;
        draw_text(fb, small, label, FOOTER_RIGHT - width, FOOTER_Y, false);
    }
}

fn finish_working_screen(fb: &mut Framebuffer, shell: &UiShell<'_>, layout: ShellLayout) {
    draw_battery_percent(fb, layout, shell.battery_percent);
}

fn hline(fb: &mut Framebuffer, x: i16, y: i16, w: i16) {
    fill_rect(fb, Rect::new(x as u16, y as u16, w as u16, 1), false);
}

fn draw_text_centered(fb: &mut Framebuffer, font: &BitmapFont, text: &str, cx: i16, y: i16) {
    let x = cx - measure_text(font, text) as i16 / 2;
    draw_text(fb, font, text, x, y, false);
}

fn fmt_numbered_chapter(number: usize, buf: &mut [u8; 32]) -> &str {
    let mut cursor = 0;
    push_str(buf, &mut cursor, "Chapter ");
    push_usize(buf, &mut cursor, number);
    core::str::from_utf8(&buf[..cursor]).unwrap_or("Chapter")
}

fn draw_text_truncated(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    x: i16,
    y: i16,
    max_w: usize,
    white: bool,
) {
    let text = fit_text(font, text, max_w.min(u16::MAX as usize) as u16);
    draw_text(fb, font, text, x, y, white);
}

/// Greedy two-line word wrap for the display-face title. Returns the
/// title's two lines; the second is empty when one line fits. The
/// second line is glyph-truncated if the remainder still overflows,
/// and a single unbreakable overlong word truncates on line one.
pub(crate) fn wrap_title<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, &'a str) {
    let (lines, _, _) = wrap_title_lines(font, text, max_w, 2);
    (lines[0], lines[1])
}

/// Break a title into up to `max_lines` lines at word boundaries, each
/// within the measure. The final line is cut to the measure when the title
/// still overflows, with room reserved for the caller's `...`; the returned
/// flag says whether that happened.
pub(crate) fn wrap_title_lines<'a>(
    font: &BitmapFont,
    text: &'a str,
    max_w: u16,
    max_lines: usize,
) -> ([&'a str; 3], usize, bool) {
    let max_lines = max_lines.clamp(1, 3);
    let mut lines = [""; 3];
    let mut count = 0;
    let mut rest = text;
    while count < max_lines - 1 && measure_text(font, rest) > max_w {
        let (line, tail) = split_title_line(font, rest, max_w);
        if tail.is_empty() {
            // An unbreakable over-wide word: the cut line is final.
            lines[count] = line;
            return (lines, count + 1, true);
        }
        lines[count] = line;
        count += 1;
        rest = tail;
    }
    if measure_text(font, rest) <= max_w {
        lines[count] = rest;
        return (lines, count + 1, false);
    }
    let dots = measure_text(font, "...");
    lines[count] = fit_text(font, rest, max_w.saturating_sub(dots));
    (lines, count + 1, true)
}

/// One title line at the widest word boundary within the measure, plus the
/// untrimmed remainder. An over-wide first word cannot break: it comes back
/// cut to the measure with an empty remainder.
fn split_title_line<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> (&'a str, &'a str) {
    let mut split = 0usize;
    for (index, _) in text.match_indices(' ') {
        if measure_text(font, &text[..index]) <= max_w {
            split = index;
        } else {
            break;
        }
    }
    if split == 0 {
        return (fit_text(font, text, max_w), "");
    }
    (&text[..split], text[split + 1..].trim_start())
}

pub(crate) fn fit_text<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> &'a str {
    if measure_text(font, text) <= max_w {
        return text;
    }
    let mut end = 0usize;
    for (index, _) in text.char_indices() {
        let candidate = &text[..index];
        if !candidate.is_empty() && measure_text(font, candidate) > max_w {
            break;
        }
        end = index;
    }
    text[..end].trim_end()
}

fn push_str(buf: &mut [u8], cursor: &mut usize, value: &str) {
    for byte in value.bytes() {
        if *cursor >= buf.len() {
            return;
        }
        buf[*cursor] = byte;
        *cursor += 1;
    }
}

fn push_usize(buf: &mut [u8], cursor: &mut usize, value: usize) {
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

fn refresh_policy_label(policy: UiRefreshPolicy) -> &'static str {
    match policy {
        UiRefreshPolicy::FastOnly => "fast only",
        UiRefreshPolicy::FullOnWake => "full on wake",
        UiRefreshPolicy::FullEveryTen => "full every ten",
    }
}

fn orientation_label(orientation: UiOrientation) -> &'static str {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => "buttons down",
        UiOrientation::LandscapeButtonsTop => "buttons up",
        UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => "portrait",
    }
}

fn front_buttons_label(pages_left: bool) -> &'static str {
    if pages_left {
        "pages left"
    } else {
        "pages right"
    }
}

fn font_size_label(size: display::font::FontSize) -> &'static str {
    match size {
        display::font::FontSize::Small => "small",
        display::font::FontSize::Medium => "medium",
        display::font::FontSize::Large => "large",
    }
}

fn line_spacing_label(spacing: display::font::LineSpacing) -> &'static str {
    match spacing {
        display::font::LineSpacing::Compact => "compact",
        display::font::LineSpacing::Normal => "normal",
        display::font::LineSpacing::Relaxed => "relaxed",
    }
}

fn font_weight_label(weight: display::font::FontWeight) -> &'static str {
    match weight {
        display::font::FontWeight::Normal => "regular",
        display::font::FontWeight::Heavy => "heavier",
    }
}

fn font_family_label(family: display::font::FontFamily, custom_name: &str) -> &str {
    match family {
        display::font::FontFamily::Literata => "literata",
        display::font::FontFamily::Merriweather => "merriweather",
        display::font::FontFamily::Custom => {
            if custom_name.is_empty() {
                "custom"
            } else {
                custom_name
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_title_lines_fits_alice_on_three_portrait_lines() {
        // The portrait content measure (444 - 44). Two lines cut this title
        // mid-word; the third line lets it render whole.
        let font = literata_display();
        let title = "Alice's Adventures in Wonderland";
        let (lines, count, overflow) = wrap_title_lines(font, title, 400, 3);
        assert_eq!(&lines[..count], &["Alice's", "Adventures in", "Wonderland"]);
        assert!(!overflow);
        for line in &lines[..count] {
            assert!(measure_text(font, line) <= 400);
        }
    }

    #[test]
    fn wrap_title_lines_flags_overflow_and_reserves_ellipsis_room() {
        let font = literata_display();
        let title = "The Collected Correspondence of an Unhurried Victorian Naturalist";
        let (lines, count, overflow) = wrap_title_lines(font, title, 400, 3);
        assert_eq!(count, 3);
        assert!(overflow);
        let dots = measure_text(font, "...");
        assert!(measure_text(font, lines[2]) + dots <= 400);
    }

    #[test]
    fn wrap_title_keeps_short_titles_on_one_line() {
        let font = literata_display();
        let (first, second) = wrap_title(font, "Dune", 530);
        assert_eq!(first, "Dune");
        assert!(second.is_empty());
    }

    #[test]
    fn wrap_title_breaks_long_titles_at_a_word_boundary() {
        let font = literata_display();
        let title = "Harry Potter and the Methods of Rationality";
        let (first, second) = wrap_title(font, title, 530);
        assert!(!second.is_empty());
        assert!(measure_text(font, first) <= 530);
        assert!(measure_text(font, second) <= 530);
        // The break consumes the separating space and loses no words up
        // to the second line's own truncation point.
        assert!(title.starts_with(first));
        assert!(title[first.len() + 1..].starts_with(second));
    }

    #[test]
    fn wrap_title_truncates_an_unbreakable_word() {
        let font = literata_display();
        let title = "Donaudampfschifffahrtsgesellschaftskapitaen";
        let (first, second) = wrap_title(font, title, 200);
        assert!(measure_text(font, first) <= 200);
        assert!(second.is_empty());
    }
}
