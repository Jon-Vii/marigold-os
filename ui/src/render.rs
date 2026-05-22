use crate::{UiCover, UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell, UiTocItem, UiView};
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, BitmapFont, FontStyle};
use display::render::{fill_rect, stroke_rect};
use display::{Rect, HEIGHT, WIDTH};

const HOME_ITEMS: [&str; 4] = ["Files", "Files", "Sync", "Settings"];
const SETTINGS_ITEMS: [&str; 3] = ["ORIENTATION", "REFRESH", "BACK TO HOME"];
pub fn render_shell(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters_landscape(fb, shell),
        UiView::Sync => render_sync(fb),
        UiView::Settings => render_settings(fb, shell),
    }
}

pub fn render_shell_overlay(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    match shell.view {
        UiView::Home => render_home(fb, shell),
        UiView::Library => render_library(fb, shell),
        UiView::Chapters => render_chapters_landscape(fb, shell),
        UiView::Sync => render_sync(fb),
        UiView::Settings => render_settings(fb, shell),
    }
}

fn render_home(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, shell.battery_percent);
    draw_dock_clean_rail(fb, 30, 58, 258, 340);
    draw_section_divider(fb, 330, 58, 340);
    draw_home_cover(fb, 448, 48, 202, 303, shell.active_book.cover);
    draw_text_centered_fit(fb, title_font, shell.active_book.title, 549, 394, 300);
    draw_text_centered_fit(fb, body_font, shell.active_book.author, 549, 424, 260);
    draw_home_progress(fb, 494, 454, 110, shell.active_book.progress_permille);
    mirror_framebuffer_long_axis(fb);
}

fn render_library(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let meta_font = literata(FontStyle::Italic);
    draw_text(fb, title_font, "Files", 58, 54, false);
    draw_text(fb, meta_font, "/books, then card root", 58, 84, false);
    draw_battery_landscape_minimal(fb, 718, 32, shell.battery_percent);
    fill_rect(fb, Rect::new(58, 104, 684, 1), false);

    match shell.library_status {
        UiLibraryStatus::NotScanned | UiLibraryStatus::Scanning => {
            mirror_framebuffer_long_axis(fb);
            return;
        }
        UiLibraryStatus::Error => {
            draw_text(fb, body_font, "Library unavailable", 58, 190, false);
            draw_text(fb, meta_font, "Storage catalog not loaded", 58, 224, false);
            mirror_framebuffer_long_axis(fb);
            return;
        }
        UiLibraryStatus::Empty => {
            draw_text(fb, body_font, "No books available", 58, 190, false);
            draw_text(fb, meta_font, "Add EPUB files to /books", 58, 224, false);
            mirror_framebuffer_long_axis(fb);
            return;
        }
        UiLibraryStatus::Ready => {}
    }

    if shell.library_entries.is_empty() {
        draw_text(fb, body_font, "No books available", 58, 190, false);
        draw_text(fb, meta_font, "Add EPUB files to /books", 58, 224, false);
        mirror_framebuffer_long_axis(fb);
        return;
    }

    let mut baseline_y = 142i16;
    for (index, entry) in shell.library_entries.iter().take(8).enumerate() {
        let selected = index == shell.selection as usize;
        if selected {
            fill_rect(fb, Rect::new(46, (baseline_y - 24) as u16, 708, 31), false);
        }
        if selected {
            draw_text(fb, body_font, ">", 60, baseline_y, true);
        }
        draw_text_truncated(fb, body_font, entry, 92, baseline_y, 620, selected);
        baseline_y += 38;
    }
    draw_text(fb, meta_font, "OK opens  Back returns", 58, 448, false);
    mirror_framebuffer_long_axis(fb);
}

fn render_settings(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let meta_font = literata(FontStyle::Italic);
    draw_text(fb, title_font, "Settings", 58, 54, false);
    draw_battery_landscape_minimal(fb, 718, 32, shell.battery_percent);
    fill_rect(fb, Rect::new(58, 104, 684, 1), false);

    let values = [
        orientation_label(shell.orientation),
        refresh_policy_label(shell.refresh_policy),
        "",
    ];
    let mut baseline_y = 156i16;
    for (index, item) in SETTINGS_ITEMS.iter().enumerate() {
        let selected = index == shell.selection as usize;
        if selected {
            fill_rect(fb, Rect::new(46, (baseline_y - 25) as u16, 708, 34), false);
        }
        if selected {
            draw_text(fb, body_font, ">", 60, baseline_y, true);
        }
        draw_text(fb, body_font, item, 92, baseline_y, selected);
        if !values[index].is_empty() {
            draw_text_truncated(fb, meta_font, values[index], 410, baseline_y, 300, selected);
        }
        baseline_y += 54;
    }
    mirror_framebuffer_long_axis(fb);
}

fn render_sync(fb: &mut Framebuffer) {
    fb.clear(true);
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let meta_font = literata(FontStyle::Italic);
    draw_text(fb, title_font, "Sync", 58, 54, false);
    fill_rect(fb, Rect::new(58, 104, 684, 1), false);
    draw_text(fb, body_font, "Not configured", 58, 190, false);
    draw_text(fb, meta_font, "Back returns", 58, 448, false);
    mirror_framebuffer_long_axis(fb);
}

fn render_chapters_landscape(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    fb.clear(true);
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    let meta_font = literata(FontStyle::Italic);
    draw_text(fb, title_font, "Chapters", 58, 54, false);
    draw_text_truncated(fb, body_font, shell.active_book.title, 58, 84, 500, false);
    draw_battery_landscape_minimal(fb, 718, 32, shell.battery_percent);
    fill_rect(fb, Rect::new(58, 104, 684, 1), false);
    if shell.chapters.is_empty() {
        draw_text(fb, body_font, "No chapters found", 58, 190, false);
        return;
    }
    let selected = (shell.selection as usize).min(shell.chapters.len().saturating_sub(1));
    let first = selected.saturating_sub(5);
    let visible_count = 9usize;
    let mut baseline_y = 142i16;
    for (index, item) in shell
        .chapters
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_count)
    {
        draw_literata_toc_item(fb, body_font, item, index == selected, baseline_y);
        baseline_y += 34;
    }
    let mut counter = [0u8; 32];
    let counter = fmt_chapter_counter(selected + 1, shell.chapters.len(), &mut counter);
    draw_text(fb, meta_font, counter, 58, 448, false);
    draw_text(fb, meta_font, "OK opens  Back returns", 516, 448, false);
    mirror_framebuffer_long_axis(fb);
}

fn draw_literata_toc_item(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    item: &UiTocItem<'_>,
    selected: bool,
    baseline_y: i16,
) {
    let indent = 58 + (item.level.saturating_sub(1) as u16 * 18);
    if selected {
        fill_rect(fb, Rect::new(46, (baseline_y - 24) as u16, 708, 31), false);
    }
    if selected {
        draw_text(fb, font, ">", 60, baseline_y, true);
    }
    draw_text_truncated(
        fb,
        font,
        item.title,
        (indent + 34) as i16,
        baseline_y,
        650usize.saturating_sub(indent as usize),
        selected,
    );
}

fn draw_dock_clean_rail(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect(fb, Rect::new(x, y, w, h), false);
    let row_h = h / HOME_ITEMS.len() as u16;
    let separator_lengths = [180u16, 206, 168];
    let font = literata(FontStyle::Regular);
    for (index, label) in HOME_ITEMS.iter().enumerate() {
        let row_y = y + index as u16 * row_h;
        let center_y = row_y + row_h / 2;
        if index > 0 {
            let sep_w = separator_lengths[index - 1].min(w.saturating_sub(58));
            let sep_x = x + 22 + (index as u16 % 2) * 10;
            fill_rect(fb, Rect::new(sep_x, row_y, sep_w, 1), false);
        }
        draw_refined_left_notch(fb, x + 10, center_y - 15, index);
        draw_text(fb, font, label, x as i16 + 46, center_y as i16 + 8, false);
        draw_refined_button_well(fb, x + w - 48, center_y - 9, index);
    }
}

fn draw_refined_left_notch(fb: &mut Framebuffer, x: u16, y: u16, index: usize) {
    let stem_h = [30u16, 24, 28, 22][index.min(3)];
    let arm_w = [18u16, 14, 20, 16][index.min(3)];
    fill_rect(fb, Rect::new(x, y + (30 - stem_h) / 2, 3, stem_h), false);
    fill_rect(fb, Rect::new(x + 6, y + 15, arm_w, 1), false);
    if index.is_multiple_of(2) {
        fill_rect(fb, Rect::new(x + 6, y + 7, 1, 16), false);
    }
}

fn draw_refined_button_well(fb: &mut Framebuffer, x: u16, y: u16, index: usize) {
    let widths = [28u16, 24, 30, 26];
    let w = widths[index.min(3)];
    let x = x + (30 - w);
    stroke_rect(fb, Rect::new(x, y, w, 18), false);
    fill_rect(fb, Rect::new(x + 5, y + 5, w - 10, 1), false);
    if index != 1 {
        fill_rect(fb, Rect::new(x + 5, y + 12, w - 10, 1), false);
    }
}

fn draw_section_divider(fb: &mut Framebuffer, x: u16, y: u16, h: u16) {
    fill_rect(fb, Rect::new(x, y, 1, h), false);
    fill_rect(fb, Rect::new(x + 5, y + 34, 1, h - 68), false);
}

fn draw_cover_art_varied(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) {
    stroke_rect(fb, Rect::new(x, y, w, h), false);
    fill_rect(fb, Rect::new(x + 12, y + 14, w - 24, 1), false);
    fill_rect(fb, Rect::new(x + 24, y + 42, w - 56, 2), false);
    fill_rect(fb, Rect::new(x + 34, y + 70, w - 72, 1), false);
    let line_specs = [
        (104u16, 30u16, 122u16, 3u16),
        (126, 44, 86, 2),
        (148, 26, 138, 3),
        (172, 58, 74, 2),
        (194, 38, 112, 2),
        (220, 50, 96, 3),
        (246, 28, 130, 1),
    ];
    for (dy, inset, line_w, line_h) in line_specs {
        if dy + 8 < h {
            fill_rect(
                fb,
                Rect::new(
                    x + inset,
                    y + dy,
                    line_w.min(w.saturating_sub(inset + 18)),
                    line_h,
                ),
                false,
            );
        }
    }
    fill_rect(fb, Rect::new(x + 30, y + h - 48, w - 72, 1), false);
    fill_rect(fb, Rect::new(x + 42, y + h - 34, w - 104, 2), false);
}

fn draw_home_cover(
    fb: &mut Framebuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    cover: Option<UiCover<'_>>,
) {
    if let Some(cover) = cover {
        if cover.width > 0 && cover.height > 0 && !cover.bits.is_empty() {
            draw_cover_bitmap(fb, x, y, w, h, cover);
            return;
        }
    }
    draw_cover_art_varied(fb, x, y, w, h);
}

fn draw_cover_bitmap(fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16, cover: UiCover<'_>) {
    stroke_rect(fb, Rect::new(x, y, w, h), false);
    let src_w = cover.width as usize;
    let src_h = cover.height as usize;
    let stride = cover.stride as usize;
    let dst_w = w.saturating_sub(4).max(1) as usize;
    let dst_h = h.saturating_sub(4).max(1) as usize;
    let scale_x = dst_w * 1024 / src_w.max(1);
    let scale_y = dst_h * 1024 / src_h.max(1);
    let scale = scale_x.min(scale_y).max(1);
    let scaled_w = (src_w * scale / 1024).max(1).min(dst_w);
    let scaled_h = (src_h * scale / 1024).max(1).min(dst_h);
    let ox = x as usize + 2 + (dst_w - scaled_w) / 2;
    let oy = y as usize + 2 + (dst_h - scaled_h) / 2;

    fill_rect(
        fb,
        Rect::new(ox as u16, oy as u16, scaled_w as u16, scaled_h as u16),
        true,
    );
    for dy in 0..scaled_h {
        let sy = dy * src_h / scaled_h;
        for dx in 0..scaled_w {
            let sx = dx * src_w / scaled_w;
            if cover_bit(cover.bits, stride, sx, sy) {
                fb.set_pixel(ox + dx, oy + dy, false);
            }
        }
    }
}

fn cover_bit(bits: &[u8], stride: usize, x: usize, y: usize) -> bool {
    let index = y.saturating_mul(stride).saturating_add(x / 8);
    let Some(byte) = bits.get(index) else {
        return false;
    };
    byte & (0x80 >> (x & 7)) != 0
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

fn draw_battery_landscape_minimal(fb: &mut Framebuffer, x: u16, y: u16, percent: u8) {
    stroke_rect(fb, Rect::new(x, y, 38, 16), false);
    fill_rect(fb, Rect::new(x + 38, y + 5, 3, 6), false);
    let fill_w = ((percent.min(100) as u16 * 30) / 100).max(1);
    fill_rect(fb, Rect::new(x + 4, y + 4, fill_w, 8), false);
}

fn draw_home_progress(fb: &mut Framebuffer, x: u16, y: u16, w: u16, permille: u16) {
    fill_rect(fb, Rect::new(x, y, w, 1), false);
    let fill_w = ((w as u32 * permille.min(1000) as u32) / 1000) as u16;
    fill_rect(
        fb,
        Rect::new(x, y.saturating_sub(1), fill_w.max(1), 3),
        false,
    );
}

fn draw_text_centered_fit(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    text: &str,
    center_x: i16,
    y: i16,
    max_w: u16,
) {
    let text = fit_text(font, text, max_w);
    let x = center_x - measure_text(font, text) as i16 / 2;
    draw_text(fb, font, text, x, y, false);
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

fn fit_text<'a>(font: &BitmapFont, text: &'a str, max_w: u16) -> &'a str {
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

fn fmt_chapter_counter(current: usize, total: usize, buf: &mut [u8; 32]) -> &str {
    let mut cursor = 0;
    push_str(buf, &mut cursor, "Chapter ");
    push_usize(buf, &mut cursor, current);
    push_str(buf, &mut cursor, " of ");
    push_usize(buf, &mut cursor, total);
    core::str::from_utf8(&buf[..cursor]).unwrap_or("")
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

fn orientation_label(orientation: UiOrientation) -> &'static str {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => "LANDSCAPE BOTTOM",
        UiOrientation::LandscapeButtonsTop => "LANDSCAPE TOP",
        UiOrientation::PortraitButtonsLeft => "PORTRAIT LEFT",
        UiOrientation::PortraitButtonsRight => "PORTRAIT RIGHT",
    }
}

fn refresh_policy_label(policy: UiRefreshPolicy) -> &'static str {
    match policy {
        UiRefreshPolicy::FastOnly => "FAST ONLY",
        UiRefreshPolicy::FullOnWake => "FULL ON WAKE",
        UiRefreshPolicy::FullEveryTen => "FULL EVERY 10",
    }
}
