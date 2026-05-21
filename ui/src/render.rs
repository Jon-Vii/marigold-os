use crate::{UiLibraryStatus, UiOrientation, UiRefreshPolicy, UiShell, UiTocItem, UiView};
use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, BitmapFont, FontStyle};
use display::render::{draw_ascii, fill_rect, glyph_5x7, stroke_rect};
use display::{Rect, HEIGHT, WIDTH};

const HOME_ITEMS: [&str; 4] = ["Read", "Files", "Sync", "Settings"];
const SETTINGS_ITEMS: [&str; 3] = ["ORIENTATION", "REFRESH", "BACK TO HOME"];
const SHELL_ORIENTATION: UiOrientation = UiOrientation::PortraitButtonsLeft;

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
    let title_font = literata(FontStyle::Bold);
    let body_font = literata(FontStyle::Regular);
    draw_battery_landscape_minimal(fb, 726, 28, shell.battery_percent);
    draw_dock_clean_rail(fb, 30, 58, 258, 340);
    draw_section_divider(fb, 330, 58, 340);
    draw_cover_art_varied(fb, 448, 48, 202, 303);
    draw_text_centered_fit(fb, title_font, shell.active_book.title, 549, 394, 300);
    draw_text_centered_fit(fb, body_font, shell.active_book.author, 549, 424, 260);
    draw_home_progress(fb, 494, 454, 110, shell.active_book.progress_permille);
}

fn render_library(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    ui.draw_ascii("FILES", 64, 72, false);
    ui.fill_rect(64, 110, 352, 2, false);
    ui.draw_ascii("/books then /", 64, 132, false);

    match shell.library_status {
        UiLibraryStatus::NotScanned | UiLibraryStatus::Scanning => {
            ui.draw_ascii("SCANNING MICROSD", 64, 216, false);
            return;
        }
        UiLibraryStatus::Error => {
            ui.draw_ascii("MICROSD NOT READY", 64, 216, false);
            ui.draw_ascii("USE FAT16/FAT32", 64, 248, false);
            return;
        }
        UiLibraryStatus::Empty => {
            ui.draw_ascii("NO EPUB FILES FOUND", 64, 216, false);
            ui.draw_ascii("PUT BOOKS IN /books", 64, 248, false);
            return;
        }
        UiLibraryStatus::Ready => {}
    }

    if shell.library_entries.is_empty() {
        ui.draw_ascii("NO EPUB FILES FOUND", 64, 216, false);
        return;
    }

    let mut y = 198;
    for (index, entry) in shell.library_entries.iter().take(9).enumerate() {
        let selected = index == shell.selection as usize;
        if selected {
            ui.fill_rect(56, y - 12, 368, 32, false);
        }
        ui.draw_ascii(if selected { ">" } else { " " }, 76, y as usize, selected);
        ui.draw_ascii(entry, 112, y as usize, selected);
        y += 48;
    }
}

fn render_settings(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    draw_menu(&mut ui, "SETTINGS", &SETTINGS_ITEMS, shell.selection);
    ui.draw_ascii("READING ORIENTATION", 64, 380, false);
    ui.draw_ascii(orientation_label(shell.orientation), 64, 408, false);
    ui.draw_ascii("REFRESH", 64, 464, false);
    ui.draw_ascii(refresh_policy_label(shell.refresh_policy), 64, 492, false);
}

fn render_sync(fb: &mut Framebuffer) {
    let mut ui = Ui::new(fb, SHELL_ORIENTATION);
    ui.draw_ascii("SYNC", centered_x_for(480, "SYNC"), 300, false);
    ui.draw_ascii(
        "NOT CONFIGURED",
        centered_x_for(480, "NOT CONFIGURED"),
        344,
        false,
    );
    ui.draw_ascii("BACK", centered_x_for(480, "BACK"), 620, false);
}

fn render_chapters_landscape(fb: &mut Framebuffer, shell: &UiShell<'_>) {
    draw_ascii(fb, "CHAPTERS", 96, 112, false);
    if shell.chapters.is_empty() {
        draw_ascii(fb, "NO CHAPTERS", 96, 168, false);
        return;
    }
    let selected = (shell.selection as usize).min(shell.chapters.len().saturating_sub(1));
    let first = selected.saturating_sub(4);
    let mut item_y = 168usize;
    for (index, item) in shell.chapters.iter().enumerate().skip(first).take(8) {
        draw_toc_item(fb, item, index == selected, item_y);
        item_y += 36;
    }
    draw_ascii(fb, "OK JUMPS TO CHAPTER", 96, 408, false);
}

fn draw_toc_item(fb: &mut Framebuffer, item: &UiTocItem<'_>, selected: bool, y: usize) {
    if selected {
        fill_rect(fb, Rect::new(88, y as u16 - 10, 624, 28), false);
    }
    draw_ascii(fb, if selected { ">" } else { " " }, 104, y, selected);
    let indent = 136 + (item.level.saturating_sub(1) as usize * 18);
    draw_ascii_truncated(
        fb,
        item.title,
        indent,
        y,
        66usize.saturating_sub(item.level as usize * 2),
        selected,
    );
}

fn draw_menu(ui: &mut Ui<'_>, title: &str, items: &[&str], selection: u8) {
    ui.draw_ascii(title, 64, 72, false);
    ui.fill_rect(64, 110, 352, 2, false);
    let mut y = 172;
    for (index, item) in items.iter().enumerate() {
        let selected = index == selection as usize;
        if selected {
            ui.fill_rect(56, y - 12, 368, 32, false);
        }
        ui.draw_ascii(if selected { ">" } else { " " }, 76, y as usize, selected);
        ui.draw_ascii(item, 112, y as usize, selected);
        y += 48;
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
    let mut cursor = x;
    for byte in text.bytes().take(max_chars) {
        let glyph = glyph_5x7(byte);
        for (col, bits) in glyph.iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    fb.set_pixel(cursor + col, y + row, inverted);
                }
            }
        }
        cursor += 8;
    }
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

fn centered_x_for(width: usize, text: &str) -> usize {
    width.saturating_sub(text.len() * 8) / 2
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

struct Ui<'a> {
    fb: &'a mut Framebuffer,
    orientation: UiOrientation,
}

impl<'a> Ui<'a> {
    fn new(fb: &'a mut Framebuffer, orientation: UiOrientation) -> Self {
        Self { fb, orientation }
    }

    fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, white: bool) {
        let y = self.logical_y_for_height(y, h);
        for yy in y..y.saturating_add(h) {
            for xx in x..x.saturating_add(w) {
                self.set_pixel(xx as usize, yy as usize, white);
            }
        }
    }

    fn draw_ascii(&mut self, text: &str, x: usize, y: usize, white: bool) {
        let y = self.logical_y_for_height(y as u16, 7) as usize;
        let mut cursor = x;
        for byte in text.bytes() {
            self.draw_glyph(byte, cursor, y, white);
            cursor += 8;
        }
    }

    fn draw_glyph(&mut self, byte: u8, x: usize, y: usize, white: bool) {
        let glyph = glyph_5x7(byte);
        for (col, bits) in glyph.iter().enumerate() {
            for row in 0..7 {
                if bits & (1 << row) != 0 {
                    self.set_pixel(x + col, y + row, white);
                }
            }
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, white: bool) {
        let Some((fx, fy)) = map_ui_pixel(self.orientation, x, y) else {
            return;
        };
        self.fb.set_pixel(fx, fy, white);
    }

    fn logical_y_for_height(&self, y: u16, h: u16) -> u16 {
        match self.orientation {
            UiOrientation::PortraitButtonsLeft | UiOrientation::PortraitButtonsRight => {
                (WIDTH as u16).saturating_sub(y.saturating_add(h))
            }
            UiOrientation::LandscapeButtonsBottom | UiOrientation::LandscapeButtonsTop => y,
        }
    }
}

fn map_ui_pixel(orientation: UiOrientation, x: usize, y: usize) -> Option<(usize, usize)> {
    match orientation {
        UiOrientation::LandscapeButtonsBottom => {
            if x < WIDTH && y < HEIGHT {
                Some((x, y))
            } else {
                None
            }
        }
        UiOrientation::LandscapeButtonsTop => {
            if x < WIDTH && y < HEIGHT {
                Some((WIDTH - 1 - x, HEIGHT - 1 - y))
            } else {
                None
            }
        }
        UiOrientation::PortraitButtonsRight => {
            if x < HEIGHT && y < WIDTH {
                Some((WIDTH - 1 - y, x))
            } else {
                None
            }
        }
        UiOrientation::PortraitButtonsLeft => {
            if x < HEIGHT && y < WIDTH {
                Some((y, HEIGHT - 1 - x))
            } else {
                None
            }
        }
    }
}
