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
use display::fb::Framebuffer;
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
/// Rows the Library list shows at once. Public so the firmware sizes the
/// resident catalog window to cover the visible range it must stream in.
pub const LIBRARY_VISIBLE_ROWS: usize = 6;
const VISIBLE_ROWS: usize = LIBRARY_VISIBLE_ROWS;
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
                content_x: WIDTH as i16 - CONTENT_RIGHT,
                content_right: WIDTH as i16 - CONTENT_X,
                colophon_right: WIDTH as i16 - (COLOPHON_RIGHT - CONTENT_RIGHT),
                heading_cx: WIDTH as i16 - HEADING_CX,
            },
            _ => Self {
                mirrored: false,
                content_x: CONTENT_X,
                content_right: CONTENT_RIGHT,
                colophon_right: COLOPHON_RIGHT,
                heading_cx: HEADING_CX,
            },
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
    let layout = ShellLayout::for_orientation(shell.orientation);
    dash_key(fb, layout, 0, "library", false);
    dash_key(fb, layout, 1, "continue", true);
    dash_key(fb, layout, 2, "wireless", false);
    dash_key(fb, layout, 3, "settings", false);

    // Long titles wrap to a second line that grows upward, keeping the
    // author/rule/colophon furniture (and one-line titles) fixed.
    let title_font = literata_display();
    let (first, second) = wrap_title(
        title_font,
        shell.active_book.title,
        layout.content_width() as u16,
    );
    if second.is_empty() {
        draw_text(fb, title_font, first, layout.content_x, 180, false);
    } else {
        draw_text(
            fb,
            title_font,
            first,
            layout.content_x,
            180 - TITLE_LEADING,
            false,
        );
        draw_text(fb, title_font, second, layout.content_x, 180, false);
    }
    if !shell.active_book.author.is_empty() {
        ls_caps(
            fb,
            literata_small(FontStyle::Regular),
            shell.active_book.author,
            layout.content_x,
            222,
            3,
        );
    }

    let permille = if shell.page_count > 1 {
        (((shell.page + 1).min(shell.page_count) as u64 * 1000) / shell.page_count as u64) as u16
    } else {
        shell.active_book.progress_permille
    };
    progress_rule(fb, layout.content_x, 280, 240, permille);

    // Colophon: the chapter name alone, in the book's italic voice —
    // the progress rule already answers "how far". Roman numeral
    // fallback when the book has no usable chapter title.
    draw_chapter_colophon(
        fb,
        shell.chapters,
        shell.chapter,
        shell.chapter_title,
        layout.content_x,
        312,
        layout.colophon_right - layout.content_x,
    );

    draw_battery_percent(fb, layout, shell.battery_percent);
    mirror_framebuffer_long_axis(fb);
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
    let layout = ShellLayout::for_orientation(shell.orientation);
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
    let start = library_scroll_start(selected_index, total);
    let window_start = shell.library_window_start as usize;
    let body = literata(FontStyle::Regular);
    let mut y = FIRST_ROW_Y;
    for row in 0..VISIBLE_ROWS {
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
pub fn library_scroll_start(selection: usize, total: usize) -> usize {
    let start = if selection >= VISIBLE_ROWS {
        selection + 1 - VISIBLE_ROWS
    } else {
        0
    };
    start.min(total.saturating_sub(VISIBLE_ROWS))
}

// The contents page uses tight index rows — a real table of contents,
// not a menu: title, dot leaders, the chapter's book page right-aligned.
const TOC_ROW_STEP: i16 = 36;
pub const TOC_VISIBLE_ROWS: usize = 9;

/// Absolute TOC index of the first visible Contents row that keeps
/// `selection` on screen. Shared by the renderer and the firmware's TOC
/// window loader so both agree on which slice must be resident.
pub fn toc_scroll_start(selection: usize, total: usize) -> usize {
    let start = if selection >= TOC_VISIBLE_ROWS {
        selection + 1 - TOC_VISIBLE_ROWS
    } else {
        0
    };
    start.min(total.saturating_sub(TOC_VISIBLE_ROWS))
}

fn render_chapters(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = ShellLayout::for_orientation(shell.orientation);
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
    let start = toc_scroll_start(selected, total);
    let window_start = shell.chapters_window_start as usize;
    let mut y = FIRST_ROW_Y;
    for row in 0..TOC_VISIBLE_ROWS {
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
    let layout = ShellLayout::for_orientation(shell.orientation);
    dash_key(fb, layout, 0, "home", false);
    dash_key(fb, layout, 1, "change", true);
    dash_key(fb, layout, 2, "previous", false);
    dash_key(fb, layout, 3, "next", false);
    heading(fb, layout, "Settings");

    index_row(
        fb,
        layout,
        "Typeface",
        font_family_label(shell.font_family, shell.custom_font_name),
        FIRST_ROW_Y,
        shell.selection == 0,
    );
    index_row(
        fb,
        layout,
        "Type size",
        font_size_label(shell.font_size),
        FIRST_ROW_Y + 64,
        shell.selection == 1,
    );
    index_row(
        fb,
        layout,
        "Type weight",
        font_weight_label(shell.font_weight),
        FIRST_ROW_Y + 128,
        shell.selection == 2,
    );
    index_row(
        fb,
        layout,
        "Line spacing",
        line_spacing_label(shell.line_spacing),
        FIRST_ROW_Y + 192,
        shell.selection == 3,
    );
    index_row(
        fb,
        layout,
        "Refresh",
        refresh_policy_label(shell.refresh_policy),
        FIRST_ROW_Y + 256,
        shell.selection == 4,
    );
    index_row(
        fb,
        layout,
        "Orientation",
        orientation_label(shell.orientation),
        FIRST_ROW_Y + 320,
        shell.selection == 5,
    );

    finish_working_screen(fb, shell, layout);
}

fn render_wireless(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let layout = ShellLayout::for_orientation(shell.orientation);
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

    let hint_y = 280;
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
    let y = KEY_YS[slot];
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, y + 8, false);
    let style = if primary {
        FontStyle::Bold
    } else {
        FontStyle::Regular
    };
    let label_font = literata_small(style);
    if layout.mirrored {
        let width = ls_width(label_font, label, 2);
        ls_caps(fb, label_font, label, dash_x - 24 - width, y + 6, 2);
    } else {
        ls_caps(fb, label_font, label, KEY_LABEL_X, y + 6, 2);
    }
}

/// An unused key keeps its bare dash: the mark stays, the word goes.
fn dash_unused(fb: &mut Framebuffer, layout: ShellLayout, slot: usize) {
    let dash_font = literata(FontStyle::Regular);
    let dash = "\u{2014}";
    let dash_x = if layout.mirrored {
        WIDTH as i16 - KEY_DASH_X - measure_text(dash_font, dash) as i16
    } else {
        KEY_DASH_X
    };
    draw_text(fb, dash_font, dash, dash_x, KEY_YS[slot] + 8, false);
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
    draw_text_centered(
        fb,
        literata(FontStyle::Italic),
        text,
        layout.heading_cx,
        230,
    );
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
        FOOTER_Y,
    );
}

fn draw_battery_percent(fb: &mut Framebuffer, layout: ShellLayout, percent: u8) {
    let mut buf = [0u8; 8];
    let mut cursor = 0;
    push_usize(&mut buf, &mut cursor, percent.min(100) as usize);
    push_str(&mut buf, &mut cursor, "%");
    let label = core::str::from_utf8(&buf[..cursor]).unwrap_or("");
    let small = literata_small(FontStyle::Regular);
    if layout.mirrored {
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
    mirror_framebuffer_long_axis(fb);
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
    if measure_text(font, text) <= max_w {
        return (text, "");
    }
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
    let rest = text[split + 1..].trim_start();
    (&text[..split], fit_text(font, rest, max_w))
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
        UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => "buttons down",
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
