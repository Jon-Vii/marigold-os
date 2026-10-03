//! Four complete UI design-language mockups rendered through the real
//! 1-bit framebuffer: Zen Minimal, The Shelf, Folio, and Cockpit.
//! Each direction renders home, library, reading, and in-book overlay.

use display::fb::Framebuffer;
use display::font::{draw_text, literata, measure_text, BitmapFont, FontStyle};
use display::render::glyph_5x7;
use display::{HEIGHT, WIDTH};
use std::fs::{create_dir_all, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::mockup_fonts_generated as mf;

const BLACK: bool = false;
const WHITE: bool = true;

pub fn write_design_mockups(out: &Path) -> std::io::Result<()> {
    let dir = out.join("design");
    create_dir_all(&dir)?;
    let screens: &[(&str, &str, fn(&mut Framebuffer))] = &[
        ("zen-1-home", "Home", zen_home),
        ("zen-2-library", "Library", zen_library),
        ("zen-3-reading", "Reading", zen_reading),
        ("zen-4-overlay", "In-book overlay", zen_overlay),
        ("shelf-1-home", "Home", shelf_home),
        ("shelf-2-library", "Library", shelf_library),
        ("shelf-3-reading", "Reading", shelf_reading),
        ("shelf-4-overlay", "In-book overlay", shelf_overlay),
        ("folio-1-home", "Home", folio_home),
        ("folio-2-library", "Library", folio_library),
        ("folio-3-reading", "Reading (chapter start)", folio_reading),
        ("folio-4-overlay", "In-book overlay", folio_overlay),
        ("cockpit-1-home", "Home", cockpit_home),
        ("cockpit-2-library", "Library", cockpit_library),
        ("cockpit-3-reading", "Reading", cockpit_reading),
        ("cockpit-4-overlay", "In-book overlay", cockpit_overlay),
        ("imprint-1-home", "Home", imprint_home),
        ("imprint-2-library", "Library", imprint_library),
        ("imprint-3-reading", "Reading", imprint_reading),
        ("imprint-4-overlay", "In-book overlay", imprint_overlay),
        ("imprintdm-1-home", "Home", imprintdm_home),
        ("imprintdm-2-library", "Library", imprintdm_library),
        (
            "imprintdm-3-reading",
            "Reading (full bleed)",
            imprintdm_reading,
        ),
        (
            "imprintdm-4-overlay",
            "In-book overlay (summoned margin)",
            imprintdm_overlay,
        ),
        ("imprintdm-5-settings", "Settings", imprintdm_settings),
        ("imprintdm-6-sync", "Sync", imprintdm_sync),
        (
            "imprintdm-7-sleep",
            "Sleep (persists on panel)",
            imprintdm_sleep,
        ),
        ("imprintdm-8-boot", "Boot", imprintdm_boot),
        (
            "marginalia-1-thumb-tabs",
            "Thumb tabs",
            marginalia_thumb_tabs,
        ),
        (
            "marginalia-2-registration",
            "Registration rule",
            marginalia_registration,
        ),
        ("marginalia-3-brackets", "Brackets", marginalia_brackets),
        (
            "marginalia-4-ruled-column",
            "Ruled column",
            marginalia_ruled_column,
        ),
        ("marginalia-5-pilcrows", "Pilcrows", marginalia_pilcrows),
        (
            "bracket-1-refined-drawn",
            "Drawn bracket, small caps",
            bracket_refined_drawn,
        ),
        (
            "bracket-2-typeset-caps",
            "Typeset bracket, small caps",
            bracket_typeset_caps,
        ),
        (
            "bracket-3-typeset-italic",
            "Typeset bracket, body italic",
            bracket_typeset_italic,
        ),
        ("bracket-4-emdash", "Em-dash, body italic", bracket_emdash),
        ("bracket-5-numerals", "Roman numerals", bracket_numerals),
        (
            "altstudy-1-library-italic",
            "Library, em-dash + body italic",
            altstudy_library_italic,
        ),
        (
            "altstudy-2-home-centered",
            "Home, centered title page",
            altstudy_home_centered,
        ),
        ("iter-1-baseline", "As flashed (baseline)", iter_baseline),
        (
            "iter-2-caps-label",
            "Caps label, roman + pages",
            iter_caps_label,
        ),
        ("iter-3-no-label", "No label, progress rule", iter_no_label),
        (
            "iter-4-pages-only",
            "Italic label, pages only",
            iter_pages_only,
        ),
        (
            "iter-5-author-caps",
            "Author in caps, progress rule",
            iter_author_caps,
        ),
        (
            "iter-6-measure",
            "The measure: 142 ——— 380 (broken metaphor)",
            iter_measure,
        ),
        (
            "iter-7-measure-fixed",
            "The measure, corrected: you-are-here",
            iter_measure_fixed,
        ),
        (
            "iter-8-chapter-name",
            "Chosen: author caps + chapter name",
            iter_chapter_name,
        ),
    ];
    for (name, _, draw) in screens {
        let mut fb = Framebuffer::new();
        fb.clear(true);
        draw(&mut fb);
        crate::fit_design_to_board(&mut fb);
        crate::write_png(&dir.join(format!("{name}.png")), &fb)?;
    }
    write_gallery_html(&dir, screens)?;
    println!(
        "Wrote {} design mockups to {}",
        screens.len(),
        dir.display()
    );
    Ok(())
}

// ---------------------------------------------------------------- sample data

const BOOK_TITLE: &str = "The Time Machine";
const BOOK_AUTHOR: &str = "H. G. Wells";
const CHAPTER_LABEL: &str = "VII";
const CHAPTER_NAME: &str = "The Palace of Green Porcelain";

const TTM_P1: &str = "The Time Traveller (for so it will be convenient to speak of him) was expounding a recondite matter to us. His pale grey eyes shone and twinkled, and his usually pale face was flushed and animated. The fire burnt brightly, and the soft radiance of the incandescent lights in the lilies of silver caught the bubbles that flashed and passed in our glasses.";
const TTM_P2: &str = "Our chairs, being his patents, embraced and caressed us rather than submitted to be sat upon, and there was that luxurious after-dinner atmosphere, when thought runs gracefully free of the trammels of precision.";
const TTM_P3: &str = "And he put it to us in this way\u{2014}marking the points with a lean forefinger\u{2014}as we sat and lazily admired his earnestness over this new paradox (as we thought it) and his fecundity.";
const TTM_P4: &str = "\u{201C}You must follow me carefully. I shall have to controvert one or two ideas that are almost universally accepted. The geometry, for instance, they taught you at school is founded on a misconception.\u{201D}";

const LIBRARY: &[(&str, &str, u16)] = &[
    ("The Time Machine", "H. G. Wells", 420),
    ("Flowers for Algernon", "Daniel Keyes", 710),
    ("The Dispossessed", "Ursula K. Le Guin", 130),
    ("Meditations", "Marcus Aurelius", 1000),
    ("Project Hail Mary", "Andy Weir", 0),
    ("Piranesi", "Susanna Clarke", 560),
    ("A Wizard of Earthsea", "Ursula K. Le Guin", 0),
    ("Stoner", "John Williams", 880),
];

const CHAPTERS: &[(&str, &str)] = &[
    ("I", "Introduction"),
    ("II", "The Machine"),
    ("III", "The Time Traveller Returns"),
    ("IV", "Time Travelling"),
    ("V", "In the Golden Age"),
    ("VI", "The Sunset of Mankind"),
    ("VII", "The Palace of Green Porcelain"),
    ("VIII", "In the Darkness"),
];

// ---------------------------------------------------------------- font access

fn body() -> &'static BitmapFont {
    literata(FontStyle::Regular)
}
fn body_italic() -> &'static BitmapFont {
    literata(FontStyle::Italic)
}
fn body_bold() -> &'static BitmapFont {
    literata(FontStyle::Bold)
}
fn small() -> &'static BitmapFont {
    &mf::SMALL_REGULAR
}
fn small_bold() -> &'static BitmapFont {
    &mf::SMALL_BOLD
}
fn small_italic() -> &'static BitmapFont {
    &mf::SMALL_ITALIC
}
fn title() -> &'static BitmapFont {
    &mf::TITLE_REGULAR
}
fn title_bold() -> &'static BitmapFont {
    &mf::TITLE_BOLD
}
fn display_font() -> &'static BitmapFont {
    &mf::DISPLAY_REGULAR
}

// ------------------------------------------------------------- draw helpers

fn put(fb: &mut Framebuffer, x: i16, y: i16, white: bool) {
    if x >= 0 && y >= 0 && (x as usize) < WIDTH && (y as usize) < HEIGHT {
        fb.set_pixel(x as usize, y as usize, white);
    }
}

fn rfill(fb: &mut Framebuffer, x: i16, y: i16, w: i16, h: i16, white: bool) {
    for yy in y..y + h {
        for xx in x..x + w {
            put(fb, xx, yy, white);
        }
    }
}

fn hline(fb: &mut Framebuffer, x: i16, y: i16, w: i16) {
    rfill(fb, x, y, w, 1, BLACK);
}

fn vline(fb: &mut Framebuffer, x: i16, y: i16, h: i16) {
    rfill(fb, x, y, 1, h, BLACK);
}

fn stroke(fb: &mut Framebuffer, x: i16, y: i16, w: i16, h: i16, white: bool) {
    rfill(fb, x, y, w, 1, white);
    rfill(fb, x, y + h - 1, w, 1, white);
    rfill(fb, x, y, 1, h, white);
    rfill(fb, x + w - 1, y, 1, h, white);
}

fn stroke_thick(fb: &mut Framebuffer, x: i16, y: i16, w: i16, h: i16, t: i16) {
    for i in 0..t {
        stroke(fb, x + i, y + i, w - 2 * i, h - 2 * i, BLACK);
    }
}

/// level 1 = 25% black, 2 = 50% checker, 3 = 75% black
fn dither(fb: &mut Framebuffer, x: i16, y: i16, w: i16, h: i16, level: u8) {
    for yy in y..y + h {
        for xx in x..x + w {
            let on = match level {
                1 => xx % 2 == 0 && yy % 2 == 0,
                2 => (xx + yy) % 2 == 0,
                _ => !(xx % 2 == 0 && yy % 2 == 0),
            };
            if on {
                put(fb, xx, yy, BLACK);
            }
        }
    }
}

fn text(fb: &mut Framebuffer, font: &BitmapFont, s: &str, x: i16, y: i16) -> i16 {
    draw_text(fb, font, s, x, y, BLACK)
}

fn text_white(fb: &mut Framebuffer, font: &BitmapFont, s: &str, x: i16, y: i16) -> i16 {
    draw_text(fb, font, s, x, y, WHITE)
}

fn center(fb: &mut Framebuffer, font: &BitmapFont, s: &str, cx: i16, y: i16) {
    let x = cx - measure_text(font, s) as i16 / 2;
    text(fb, font, s, x, y);
}

fn right(fb: &mut Framebuffer, font: &BitmapFont, s: &str, rx: i16, y: i16) {
    let x = rx - measure_text(font, s) as i16;
    text(fb, font, s, x, y);
}

fn ls_width(font: &BitmapFont, s: &str, extra: i16) -> i16 {
    let n = s.chars().count().saturating_sub(1) as i16;
    measure_text(font, s) as i16 + n * extra
}

/// Letterspaced all-caps text, the small-caps stand-in for this bitmap setup.
fn ls_caps(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    s: &str,
    x: i16,
    y: i16,
    extra: i16,
    white: bool,
) -> i16 {
    let upper = s.to_uppercase();
    let mut cx = x;
    for ch in upper.chars() {
        let cs = ch.to_string();
        cx = draw_text(fb, font, &cs, cx, y, white) + extra;
    }
    cx - extra
}

fn ls_caps_center(fb: &mut Framebuffer, font: &BitmapFont, s: &str, cx: i16, y: i16, extra: i16) {
    let upper = s.to_uppercase();
    let x = cx - ls_width(font, &upper, extra) / 2;
    ls_caps(fb, font, s, x, y, extra, BLACK);
}

/// Greedy word wrap with per-line x/width, optional justification.
/// Returns the baseline y for the line after the paragraph.
fn flow_para(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    content: &str,
    start_y: i16,
    lh: i16,
    line_x: impl Fn(usize) -> i16,
    line_w: impl Fn(usize) -> i16,
    justify: bool,
) -> i16 {
    let space = measure_text(font, " ") as i16;
    let mut lines: Vec<Vec<&str>> = Vec::new();
    let mut line: Vec<&str> = Vec::new();
    let mut w = 0i16;
    for word in content.split_whitespace() {
        let ww = measure_text(font, word) as i16;
        let cand = if line.is_empty() { ww } else { w + space + ww };
        if !line.is_empty() && cand > line_w(lines.len()) {
            lines.push(std::mem::take(&mut line));
            line.push(word);
            w = ww;
        } else {
            line.push(word);
            w = cand;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    let total = lines.len();
    let mut y = start_y;
    for (i, line) in lines.iter().enumerate() {
        let x = line_x(i);
        let gaps = line.len().saturating_sub(1) as i16;
        if justify && i + 1 != total && gaps > 0 {
            let ink: i16 = line.iter().map(|wd| measure_text(font, wd) as i16).sum();
            let extra = line_w(i) - ink;
            let mut cx = x;
            for (j, wd) in line.iter().enumerate() {
                cx = draw_text(fb, font, wd, cx, y, BLACK);
                if (j as i16) < gaps {
                    let mut adv = extra / gaps;
                    if (j as i16) < extra % gaps {
                        adv += 1;
                    }
                    cx += adv;
                }
            }
        } else {
            let mut cx = x;
            for wd in line.iter() {
                cx = draw_text(fb, font, wd, cx, y, BLACK) + space;
            }
        }
        y += lh;
    }
    y
}

fn para(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    content: &str,
    x: i16,
    y: i16,
    w: i16,
    lh: i16,
    justify: bool,
) -> i16 {
    flow_para(fb, font, content, y, lh, |_| x, |_| w, justify)
}

fn dropcap_para(
    fb: &mut Framebuffer,
    font: &BitmapFont,
    dc_font: &BitmapFont,
    content: &str,
    x: i16,
    y: i16,
    w: i16,
    lh: i16,
) -> i16 {
    let mut chars = content.chars();
    let Some(first) = chars.next() else { return y };
    let rest = chars.as_str().trim_start();
    let dc = first.to_string();
    let dc_adv = measure_text(dc_font, &dc) as i16 + 12;
    draw_text(fb, dc_font, &dc, x, y + lh - 2, BLACK);
    flow_para(
        fb,
        font,
        rest,
        y,
        lh,
        move |i| if i < 2 { x + dc_adv } else { x },
        move |i| if i < 2 { w - dc_adv } else { w },
        true,
    )
}

/// 1px rail with a 3px filled head: the quietest possible progress indicator.
fn progress_hairline(fb: &mut Framebuffer, x: i16, y: i16, w: i16, permille: u16) {
    hline(fb, x, y, w);
    let fill = (w as i32 * permille.min(1000) as i32 / 1000) as i16;
    rfill(fb, x, y - 1, fill.max(2), 3, BLACK);
}

fn progress_bar(fb: &mut Framebuffer, x: i16, y: i16, w: i16, h: i16, permille: u16) {
    stroke(fb, x, y, w, h, BLACK);
    let fill = ((w - 4) as i32 * permille.min(1000) as i32 / 1000) as i16;
    rfill(fb, x + 2, y + 2, fill.max(1), h - 4, BLACK);
}

fn battery_micro(fb: &mut Framebuffer, x: i16, y: i16, percent: u8) {
    stroke(fb, x, y, 22, 11, BLACK);
    rfill(fb, x + 22, y + 3, 2, 5, BLACK);
    let fill = (16 * percent.min(100) as i16) / 100;
    rfill(fb, x + 3, y + 3, fill.max(1), 5, BLACK);
}

fn local_5x7(byte: u8) -> [u8; 5] {
    match byte {
        b'%' => [0x62, 0x64, 0x08, 0x13, 0x23],
        b'+' => [0x08, 0x08, 0x3E, 0x08, 0x08],
        _ => glyph_5x7(byte),
    }
}

/// Scaled 5x7 terminal text for the Cockpit direction. The stock glyph
/// table stores bit 6 as the top row, hence the flipped bit index.
fn big_ascii(fb: &mut Framebuffer, s: &str, x: i16, y: i16, scale: i16, white: bool) -> i16 {
    let mut cx = x;
    for byte in s.bytes() {
        let glyph = local_5x7(byte);
        for (col, bits) in glyph.iter().enumerate() {
            for row in 0..7i16 {
                if bits & (1 << (6 - row)) != 0 {
                    rfill(
                        fb,
                        cx + col as i16 * scale,
                        y + row * scale,
                        scale,
                        scale,
                        white,
                    );
                }
            }
        }
        cx += 6 * scale;
    }
    cx
}

fn big_ascii_width(s: &str, scale: i16) -> i16 {
    s.len() as i16 * 6 * scale - scale
}

fn big_ascii_center(fb: &mut Framebuffer, s: &str, cx: i16, y: i16, scale: i16, white: bool) {
    big_ascii(fb, s, cx - big_ascii_width(s, scale) / 2, y, scale, white);
}

// ------------------------------------------------------------- mock covers

fn cover(
    fb: &mut Framebuffer,
    x: i16,
    y: i16,
    w: i16,
    h: i16,
    title_text: &str,
    author: &str,
    kind: u8,
) {
    rfill(fb, x, y, w, h, WHITE);
    stroke(fb, x, y, w, h, BLACK);
    if w >= 110 {
        stroke(fb, x + 5, y + 5, w - 10, h - 10, BLACK);
    }

    let band_y = y + h * 44 / 100;
    let band_h = h * 22 / 100;
    match kind % 6 {
        0 => {
            for (i, (frac, t)) in [(62i16, 3i16), (40, 2), (72, 3)].iter().enumerate() {
                let lw = w * frac / 100;
                rfill(fb, x + (w - lw) / 2, band_y + i as i16 * 12, lw, *t, BLACK);
            }
        }
        1 => {
            let n = 7i16;
            let bar = 4i16;
            let gap = 8i16;
            let total = n * bar + (n - 1) * gap;
            let sx = x + (w - total) / 2;
            for i in 0..n {
                rfill(fb, sx + i * (bar + gap), band_y, bar, band_h, BLACK);
            }
        }
        2 => {
            dither(fb, x + 12, band_y, w - 24, band_h, 2);
            let sq = 26.min(band_h - 4);
            rfill(
                fb,
                x + (w - sq) / 2,
                band_y + (band_h - sq) / 2,
                sq,
                sq,
                BLACK,
            );
        }
        3 => {
            for i in 0..3i16 {
                stroke(
                    fb,
                    x + 18 + i * 10,
                    band_y + i * 8,
                    w - 36 - i * 20,
                    band_h - i * 16,
                    BLACK,
                );
            }
        }
        4 => {
            let r = (band_h / 2).min(34);
            let cx = x + w / 2;
            let cy = band_y + band_h / 2;
            for dy in -r..=r {
                let half = r - dy.abs();
                rfill(fb, cx - half, cy + dy, half * 2 + 1, 1, BLACK);
            }
        }
        _ => {
            let cell = 8i16;
            let bw = ((w - 28) / (cell * 2)) * cell * 2;
            let sx = x + (w - bw) / 2;
            for row in 0..(band_h / cell) {
                for colu in 0..(bw / cell) {
                    if (row + colu) % 2 == 0 {
                        rfill(fb, sx + colu * cell, band_y + row * cell, cell, cell, BLACK);
                    }
                }
            }
        }
    }

    if w >= 110 {
        let tf: &BitmapFont = if w >= 190 { title_bold() } else { small_bold() };
        let upper = title_text.to_uppercase();
        let lh = tf.line_height as i16 + 2;
        let mut lines: Vec<String> = Vec::new();
        let space = measure_text(tf, " ") as i16;
        let mut cur = String::new();
        let mut cw = 0i16;
        for word in upper.split_whitespace() {
            let ww = measure_text(tf, word) as i16;
            let cand = if cur.is_empty() { ww } else { cw + space + ww };
            if !cur.is_empty() && cand > w - 28 {
                lines.push(std::mem::take(&mut cur));
                cur = word.to_string();
                cw = ww;
            } else {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(word);
                cw = cand;
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        }
        let mut ty = y + h * 12 / 100 + tf.line_height as i16;
        for line in &lines {
            center(fb, tf, line, x + w / 2, ty);
            ty += lh;
        }
        if h >= 220 && !author.is_empty() {
            center(fb, small_italic(), author, x + w / 2, y + h - 20);
        }
    } else {
        // tiny cover: initials of the significant words only
        let initials: String = title_text
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .filter(|c| c.is_uppercase())
            .collect();
        center(fb, small_bold(), &initials, x + w / 2, y + 26);
    }
}

// =====================================================================
// DIRECTION 1 — ZEN MINIMAL ("Still Water")
// One thing per screen. No boxes. Type floats in whitespace.
// =====================================================================

fn zen_home(fb: &mut Framebuffer) {
    battery_micro(fb, 754, 26, 82);

    center(fb, display_font(), BOOK_TITLE, 400, 208);
    center(fb, body_italic(), BOOK_AUTHOR, 400, 254);

    progress_hairline(fb, 310, 312, 180, 420);
    center(fb, small(), "page 142 of 380", 400, 348);

    // whisper nav mapped to Prev / Confirm / Next
    let words = ["library", "continue", "settings"];
    let centers = [200i16, 400, 600];
    for (i, word) in words.iter().enumerate() {
        center(fb, small(), word, centers[i], 446);
        if i == 1 {
            let w = measure_text(small(), word) as i16;
            hline(fb, centers[i] - w / 2, 456, w);
        }
    }
}

fn zen_library(fb: &mut Framebuffer) {
    battery_micro(fb, 754, 26, 82);

    // entry 0 (unselected)
    center(fb, body(), LIBRARY[0].0, 400, 104);
    // entry 1 (selected, blooms open)
    center(fb, title(), LIBRARY[1].0, 400, 188);
    center(fb, small_italic(), LIBRARY[1].1, 400, 220);
    progress_hairline(fb, 340, 244, 120, LIBRARY[1].2);
    // entries 2..4 (unselected)
    center(fb, body(), LIBRARY[2].0, 400, 312);
    center(fb, body(), LIBRARY[3].0, 400, 366);
    center(fb, body(), LIBRARY[4].0, 400, 420);

    page_dot(fb, 384, 452, true);
    page_dot(fb, 400, 452, false);
    page_dot(fb, 416, 452, false);
}

fn page_dot(fb: &mut Framebuffer, cx: i16, cy: i16, filled: bool) {
    let widths = [3i16, 5, 7, 7, 7, 5, 3];
    for (i, w) in widths.iter().enumerate() {
        rfill(fb, cx - w / 2, cy - 3 + i as i16, *w, 1, BLACK);
    }
    if !filled {
        let inner = [1i16, 3, 3, 3, 1];
        for (i, w) in inner.iter().enumerate() {
            rfill(fb, cx - w / 2, cy - 2 + i as i16, *w, 1, WHITE);
        }
    }
}

fn zen_reading(fb: &mut Framebuffer) {
    let x = 110;
    let w = 580;
    let mut y = 78;
    y = para(fb, body(), TTM_P1, x, y, w, 30, true);
    y += 14;
    para(fb, body(), TTM_P2, x, y, w, 30, true);
}

fn zen_overlay(fb: &mut Framebuffer) {
    zen_reading(fb);
    // summoned chrome: clean band rises from the bottom margin,
    // erasing whole text lines so nothing is sliced mid-glyph
    rfill(fb, 0, 368, 800, 112, WHITE);
    hline(fb, 110, 396, 580);

    let meta = format!("{CHAPTER_LABEL} \u{00B7} {CHAPTER_NAME}   \u{2014}   page 142 of 380");
    center(fb, small(), &meta, 400, 424);

    let words = ["chapters", "bookmarks", "library", "settings"];
    let centers = [170i16, 325, 480, 630];
    for (i, word) in words.iter().enumerate() {
        center(fb, small(), word, centers[i], 456);
        if i == 0 {
            let w = measure_text(small(), word) as i16;
            hline(fb, centers[i] - w / 2, 466, w);
        }
    }
}

// =====================================================================
// DIRECTION 2 — THE SHELF (book-forward)
// Covers are the heroes. The library is a place.
// =====================================================================

fn shelf_home(fb: &mut Framebuffer) {
    right(fb, small(), "14:32", 736, 38);
    battery_micro(fb, 748, 28, 82);

    cover(fb, 64, 60, 232, 348, BOOK_TITLE, BOOK_AUTHOR, 0);

    let x = 348;
    ls_caps(fb, small_bold(), "Continue Reading", x, 100, 2, BLACK);
    text(fb, title_bold(), BOOK_TITLE, x, 148);
    text(fb, body(), BOOK_AUTHOR, x, 182);

    progress_bar(fb, x, 206, 320, 8, 420);
    let line = format!("Chapter {CHAPTER_LABEL} \u{00B7} 42%");
    text(fb, small(), &line, x, 240);
    text(fb, small_italic(), "about 3 hr 20 min left in book", x, 264);

    hline(fb, x, 298, 392);

    ls_caps(fb, small_bold(), "On the Shelf", x, 330, 2, BLACK);
    let minis = [1usize, 2, 3];
    for (i, idx) in minis.iter().enumerate() {
        let mx = x + i as i16 * 112;
        cover(fb, mx, 344, 88, 110, LIBRARY[*idx].0, "", (*idx % 6) as u8);
        progress_hairline(fb, mx, 462, 88, LIBRARY[*idx].2);
    }
}

fn shelf_library(fb: &mut Framebuffer) {
    text(fb, title_bold(), "Library", 64, 62);
    right(fb, small(), "8 books \u{00B7} recent first", 736, 60);
    hline(fb, 64, 80, 672);

    let xs = [64i16, 238, 412, 586];
    for (i, x) in xs.iter().enumerate() {
        let (t, a, p) = LIBRARY[i];
        if i == 0 {
            rfill(fb, x + 6, 110, 150, 225, BLACK); // soft shadow
            cover(fb, *x, 104, 150, 225, t, "", (i % 6) as u8);
            stroke_thick(fb, x - 3, 101, 156, 231, 2);
        } else {
            cover(fb, *x, 104, 150, 225, t, "", (i % 6) as u8);
        }
        // label block
        let cxm = x + 75;
        let mut lines: Vec<&str> = vec![t];
        if measure_text(small(), t) as i16 > 150 {
            lines = t.splitn(2, ' ').collect();
        }
        let mut ty = 360;
        for line in &lines {
            center(fb, small(), line, cxm, ty);
            ty += 20;
        }
        center(fb, small_italic(), a, cxm, ty);
        if p > 0 {
            progress_hairline(fb, x + 30, ty + 16, 90, p);
        }
    }

    center(fb, small(), "page 1 of 2  \u{2192}", 400, 458);
}

fn shelf_reading(fb: &mut Framebuffer) {
    ls_caps_center(fb, small(), BOOK_TITLE, 400, 38, 3);

    let x = 90;
    let w = 620;
    let mut y = 86;
    y = para(fb, body(), TTM_P1, x, y, w, 30, true);
    y += 12;
    para(fb, body(), TTM_P2, x, y, w, 30, true);

    // footer: chapter-ticked progress rail
    let bar_y = 432;
    stroke(fb, x, bar_y, w, 6, BLACK);
    rfill(fb, x, bar_y, (w as i32 * 420 / 1000) as i16, 6, BLACK);
    for frac in [14i32, 31, 47, 62, 74, 88] {
        let tx = x + (w as i32 * frac / 100) as i16;
        vline(fb, tx, bar_y - 3, 12);
    }
    text(fb, small(), "42%", x, 464);
    let mid = format!("{CHAPTER_LABEL}. {CHAPTER_NAME}");
    center(fb, small(), &mid, 400, 464);
    right(fb, small(), "142 of 380", x + w, 464);
}

fn shelf_overlay(fb: &mut Framebuffer) {
    shelf_reading(fb);
    // bookmark panel slides in from the right
    rfill(fb, 500, 0, 300, 480, WHITE);
    vline(fb, 500, 0, 480);
    vline(fb, 502, 0, 480);
    // ribbon
    rfill(fb, 742, 0, 26, 44, BLACK);
    for step in 0..6i16 {
        rfill(fb, 742 + step * 2, 44 + step * 2, 26 - step * 4, 2, BLACK);
    }

    ls_caps(fb, small_bold(), "Contents", 528, 52, 2, BLACK);
    hline(fb, 528, 66, 244);

    let mut y = 104;
    for (num, name) in CHAPTERS {
        let current = *num == CHAPTER_LABEL;
        let font = if current { small_bold() } else { small() };
        if current {
            text(fb, small_bold(), "\u{2192}", 508, y);
        }
        let label = format!("{num}. {name}");
        let mut shown = label.clone();
        while measure_text(font, &shown) as i16 > 246 && shown.len() > 4 {
            shown.truncate(shown.len() - 1);
        }
        if shown.len() < label.len() {
            shown.push('\u{2026}');
        }
        text(fb, font, &shown, 528, y);
        y += 42;
    }
}

// =====================================================================
// DIRECTION 3 — FOLIO (editorial print)
// The UI is typeset like a fine book: rules, small caps, folios.
// =====================================================================

fn folio_home(fb: &mut Framebuffer) {
    ls_caps_center(fb, small(), "Xteink X4", 400, 40, 6);
    rfill(fb, 240, 52, 320, 2, BLACK);
    hline(fb, 240, 58, 320);

    center(fb, small_italic(), "now reading", 400, 148);
    center(fb, title(), BOOK_TITLE, 400, 204);
    center(fb, small_italic(), "by H. G. Wells", 400, 238);

    let colophon = format!("Chapter {CHAPTER_LABEL} \u{00B7} Page 142 of 380 \u{00B7} 42 per cent");
    center(fb, small(), &colophon, 400, 294);

    hline(fb, 330, 332, 140);

    let entries = ["I. Continue", "II. Library", "III. Settings"];
    let centers = [170i16, 400, 630];
    for (i, entry) in entries.iter().enumerate() {
        center(fb, body(), entry, centers[i], 410);
        if i == 0 {
            let w = measure_text(body(), entry) as i16;
            hline(fb, centers[i] - w / 2, 420, w);
        }
    }

    center(
        fb,
        small(),
        "\u{00B7} 82% \u{00B7} 14:32 \u{00B7}",
        400,
        458,
    );
}

fn folio_library(fb: &mut Framebuffer) {
    ls_caps_center(fb, small(), "Library", 400, 40, 5);
    hline(fb, 90, 52, 620);
    rfill(fb, 90, 55, 620, 2, BLACK);

    let mut y = 112;
    for (i, (t, a, p)) in LIBRARY.iter().take(5).enumerate() {
        if i == 2 {
            text(fb, body(), "\u{2192}", 88, y);
        }
        let end_x = text(fb, body(), t, 120, y);
        let pct = if *p == 1000 {
            "read".to_string()
        } else if *p == 0 {
            "new".to_string()
        } else {
            format!("{}%", p / 10)
        };
        let pct_w = measure_text(body(), &pct) as i16;
        // dot leaders
        let mut dx = end_x + 16;
        while dx < 680 - pct_w - 14 {
            put(fb, dx, y - 2, BLACK);
            dx += 8;
        }
        right(fb, body(), &pct, 680, y);
        text(fb, small_italic(), a, 120, y + 22);
        y += 64;
    }

    center(fb, small(), "\u{2013} 1 of 2 \u{2013}", 400, 456);
}

fn folio_reading(fb: &mut Framebuffer) {
    // chapter-opening pages drop the running head, as in print
    center(fb, title(), CHAPTER_LABEL, 400, 110);
    hline(fb, 388, 126, 24);
    ls_caps_center(fb, small(), CHAPTER_NAME, 400, 158, 4);

    let x = 90;
    let w = 620;
    let y = 212;
    dropcap_para(fb, body(), display_font(), TTM_P1, x, y, w, 30);

    center(fb, small(), "\u{2013} 142 \u{2013}", 400, 456);
}

fn folio_overlay(fb: &mut Framebuffer) {
    ls_caps(fb, small(), BOOK_TITLE, 90, 38, 3, BLACK);
    hline(fb, 90, 50, 620);
    let x = 90;
    let w = 620;
    let mut y = 88;
    y = para(fb, body(), TTM_P3, x, y, w, 30, true);
    y += 10;
    para(fb, body(), TTM_P4, x, y, w, 30, true);

    // footnote separator + apparatus
    hline(fb, 90, 366, 140);
    let meta = format!(
        "{CHAPTER_LABEL} \u{00B7} {CHAPTER_NAME} \u{2014} page 142 of 380 \u{00B7} 42 per cent"
    );
    text(fb, small(), &meta, 90, 394);

    let items = ["1. Chapters", "2. Bookmarks", "3. Library", "4. Settings"];
    let mut ix = 90i16;
    for (i, item) in items.iter().enumerate() {
        let end = text(fb, body(), item, ix, 432);
        if i == 0 {
            hline(fb, ix, 442, end - ix);
        }
        ix = end + 38;
    }
    right(fb, small(), "82% \u{00B7} 14:32", 710, 462);
}

// =====================================================================
// DIRECTION 4 — COCKPIT (instrument panel)
// Honest hardware. Dense data, gauges, terminal type, inverse bars.
// =====================================================================

fn cockpit_status_bar(fb: &mut Framebuffer) {
    rfill(fb, 0, 0, 800, 32, BLACK);
    big_ascii(fb, "XTEINK X4", 12, 9, 2, WHITE);
    big_ascii_center(fb, "14:32", 400, 9, 2, WHITE);
    big_ascii(fb, "SD 12.4GB", 540, 9, 2, WHITE);
    big_ascii(fb, "82%", 690, 9, 2, WHITE);
    stroke(fb, 736, 9, 30, 14, WHITE);
    rfill(fb, 766, 13, 3, 6, WHITE);
    rfill(fb, 739, 12, 20, 8, WHITE);
}

fn cockpit_home(fb: &mut Framebuffer) {
    cockpit_status_bar(fb);

    big_ascii(fb, "NOW READING", 20, 52, 2, BLACK);
    hline(fb, 20, 72, 132);

    cover(fb, 20, 88, 96, 144, BOOK_TITLE, "", 0);
    text(fb, body_bold(), BOOK_TITLE, 132, 116);
    text(fb, small(), BOOK_AUTHOR, 132, 142);
    let ch = format!("Chapter {CHAPTER_LABEL} of XVI");
    text(fb, small(), &ch, 132, 166);
    text(fb, small_italic(), CHAPTER_NAME, 132, 190);

    // master gauge
    stroke_thick(fb, 20, 256, 364, 26, 2);
    rfill(fb, 24, 260, (356 * 420 / 1000) as i16, 18, BLACK);
    for i in 1..10i16 {
        vline(fb, 20 + i * 36, 282, 5);
    }
    big_ascii(fb, "42%", 20, 304, 5, BLACK);
    big_ascii(fb, "PAGE 142/380", 130, 308, 2, BLACK);
    big_ascii(fb, "REMAIN 3H 20M", 130, 330, 2, BLACK);

    vline(fb, 408, 48, 372);

    big_ascii(fb, "STATS", 428, 52, 2, BLACK);
    hline(fb, 428, 72, 60);
    big_ascii(fb, "SESSION   47 MIN", 428, 88, 2, BLACK);
    big_ascii(fb, "TODAY     1H 22M", 428, 110, 2, BLACK);
    big_ascii(fb, "STREAK    6 DAYS", 428, 132, 2, BLACK);
    big_ascii(fb, "PAGES/HR      38", 428, 154, 2, BLACK);
    big_ascii(fb, "FINISHED       3", 428, 176, 2, BLACK);

    big_ascii(fb, "PAGES/DAY", 428, 218, 2, BLACK);
    let heights = [60i16, 95, 40, 120, 80, 30, 140];
    let labels = ["M", "T", "W", "T", "F", "S", "S"];
    for (i, h) in heights.iter().enumerate() {
        let bx = 428 + i as i16 * 48;
        let by = 388 - h;
        if i == 6 {
            rfill(fb, bx, by, 34, *h, BLACK);
        } else {
            stroke(fb, bx, by, 34, *h, BLACK);
            dither(fb, bx + 1, by + 1, 32, h - 2, 2);
        }
        big_ascii(fb, labels[i], bx + 11, 396, 2, BLACK);
    }

    // button legend
    let legends = ["BACK:LIBRARY", "PREV:BOOK-", "OK:READ", "NEXT:BOOK+"];
    for (i, label) in legends.iter().enumerate() {
        let bx = 20 + i as i16 * 196;
        rfill(fb, bx, 438, 180, 30, BLACK);
        big_ascii_center(fb, label, bx + 90, 446, 2, WHITE);
    }
}

fn cockpit_library(fb: &mut Framebuffer) {
    cockpit_status_bar(fb);

    big_ascii(fb, "NO", 24, 56, 2, BLACK);
    big_ascii(fb, "TITLE", 70, 56, 2, BLACK);
    big_ascii(fb, "AUTHOR", 420, 56, 2, BLACK);
    big_ascii(fb, "PCT", 620, 56, 2, BLACK);
    big_ascii(fb, "SIZE", 700, 56, 2, BLACK);
    hline(fb, 16, 78, 768);

    let sizes = [
        "0.2M", "0.5M", "0.8M", "0.1M", "1.4M", "0.6M", "0.4M", "0.3M",
    ];
    for (i, (t, a, p)) in LIBRARY.iter().enumerate() {
        let y = 94 + i as i16 * 38;
        let selected = i == 1;
        if selected {
            rfill(fb, 16, y - 6, 768, 32, BLACK);
        }
        let white = selected;
        let num = format!("{:02}", i + 1);
        big_ascii(fb, &num, 24, y, 2, white);
        let mut t28: String = t.to_uppercase();
        t28.truncate(28);
        big_ascii(fb, &t28, 70, y, 2, white);
        let mut a15: String = a.to_uppercase();
        a15.truncate(15);
        big_ascii(fb, &a15, 420, y, 2, white);
        let pct = if *p == 1000 {
            "DONE".to_string()
        } else {
            format!("{:3}", p / 10)
        };
        big_ascii(fb, &pct, 614, y, 2, white);
        big_ascii(fb, sizes[i], 700, y, 2, white);
    }

    hline(fb, 16, 402, 768);
    big_ascii(fb, "8 BOOKS  28.4 MB  SORT:RECENT", 24, 414, 2, BLACK);

    let legends = ["BACK:HOME", "PREV:UP", "OK:OPEN", "NEXT:DOWN"];
    for (i, label) in legends.iter().enumerate() {
        let bx = 20 + i as i16 * 196;
        rfill(fb, bx, 438, 180, 30, BLACK);
        big_ascii_center(fb, label, bx + 90, 446, 2, WHITE);
    }
}

fn cockpit_reading(fb: &mut Framebuffer) {
    rfill(fb, 0, 0, 800, 26, BLACK);
    big_ascii(fb, "THE TIME MACHINE", 12, 6, 2, WHITE);
    big_ascii(fb, "CH 7/16", 696, 6, 2, WHITE);

    let x = 56;
    let w = 688;
    let mut y = 68;
    y = para(fb, body(), TTM_P1, x, y, w, 30, true);
    y += 10;
    y = para(fb, body(), TTM_P2, x, y, w, 30, true);
    y += 10;
    para(fb, body(), TTM_P3, x, y, w, 30, true);

    rfill(fb, 0, 448, 800, 32, BLACK);
    stroke(fb, 12, 456, 220, 16, WHITE);
    rfill(fb, 14, 458, (216 * 420 / 1000) as i16, 12, WHITE);
    big_ascii(fb, "P 142/380  42%", 260, 458, 2, WHITE);
    big_ascii(fb, "14:32  BAT 82%", 600, 458, 2, WHITE);
}

fn cockpit_overlay(fb: &mut Framebuffer) {
    cockpit_reading(fb);

    let px = 220i16;
    let py = 90i16;
    let pw = 360i16;
    let ph = 300i16;
    rfill(fb, px, py, pw, ph, WHITE);
    stroke_thick(fb, px, py, pw, ph, 3);
    rfill(fb, px, py, pw, 30, BLACK);
    big_ascii_center(fb, "MENU", px + pw / 2, py + 8, 2, WHITE);

    let items = ["CHAPTERS", "BOOKMARKS", "GO TO PAGE", "LIBRARY", "SETTINGS"];
    for (i, item) in items.iter().enumerate() {
        let iy = py + 52 + i as i16 * 40;
        if i == 0 {
            rfill(fb, px + 12, iy - 8, pw - 24, 32, BLACK);
            big_ascii(fb, ">", px + 24, iy, 2, WHITE);
            big_ascii(fb, item, px + 52, iy, 2, WHITE);
        } else {
            big_ascii(fb, item, px + 52, iy, 2, BLACK);
        }
    }
    hline(fb, px + 12, py + ph - 38, pw - 24);
    big_ascii(fb, "OK:SELECT  BACK:CLOSE", px + 24, py + ph - 26, 2, BLACK);
}

// =====================================================================
// DIRECTION 5 — IMPRINT (the blend)
// Folio's typographic logic everywhere; Zen's restraint inside the book.
// No masthead, no running heads — but folios, dot leaders, small caps,
// and footnote-style apparatus where chrome must exist.
// =====================================================================

fn imprint_home(fb: &mut Framebuffer) {
    center(fb, small_italic(), "now reading", 400, 130);
    center(fb, display_font(), BOOK_TITLE, 400, 196);
    center(fb, small_italic(), "by H. G. Wells", 400, 234);

    let colophon = format!("Chapter {CHAPTER_LABEL} \u{00B7} Page 142 of 380 \u{00B7} 42 per cent");
    center(fb, small(), &colophon, 400, 290);

    hline(fb, 330, 326, 140);

    let entries = ["I. Continue", "II. Library", "III. Settings"];
    let centers = [170i16, 400, 630];
    for (i, entry) in entries.iter().enumerate() {
        center(fb, body(), entry, centers[i], 410);
        if i == 0 {
            let w = measure_text(body(), entry) as i16;
            hline(fb, centers[i] - w / 2, 420, w);
        }
    }

    center(
        fb,
        small(),
        "\u{00B7} 82% \u{00B7} 14:32 \u{00B7}",
        400,
        458,
    );
}

fn imprint_library(fb: &mut Framebuffer) {
    ls_caps_center(fb, small(), "Library", 400, 42, 5);
    hline(fb, 240, 56, 320);

    let mut y = 118;
    for (i, (t, a, p)) in LIBRARY.iter().take(5).enumerate() {
        if i == 2 {
            text(fb, body(), "\u{2192}", 88, y);
        }
        let end_x = text(fb, body(), t, 120, y);
        let pct = if *p == 1000 {
            "read".to_string()
        } else if *p == 0 {
            "new".to_string()
        } else {
            format!("{}%", p / 10)
        };
        let pct_w = measure_text(body(), &pct) as i16;
        let mut dx = end_x + 16;
        while dx < 680 - pct_w - 14 {
            put(fb, dx, y - 2, BLACK);
            dx += 8;
        }
        right(fb, body(), &pct, 680, y);
        text(fb, small_italic(), a, 120, y + 22);
        y += 64;
    }

    center(fb, small(), "\u{2013} 1 of 2 \u{2013}", 400, 456);
}

fn imprint_reading(fb: &mut Framebuffer) {
    // Zen page, Folio folio: nothing but the text and a page numeral
    let x = 110;
    let w = 580;
    let mut y = 78;
    y = para(fb, body(), TTM_P1, x, y, w, 30, true);
    y += 14;
    para(fb, body(), TTM_P2, x, y, w, 30, true);

    center(fb, small(), "\u{2013} 142 \u{2013}", 400, 458);
}

fn imprint_overlay(fb: &mut Framebuffer) {
    imprint_reading(fb);
    // summoned chrome set as a footnote: short separator rule at the
    // left margin, apparatus below, numbered menu entries
    rfill(fb, 0, 368, 800, 112, WHITE);
    hline(fb, 110, 388, 140);
    right(fb, small(), "82% \u{00B7} 14:32", 690, 392);

    let meta = format!(
        "{CHAPTER_LABEL} \u{00B7} {CHAPTER_NAME} \u{2014} page 142 of 380 \u{00B7} 42 per cent"
    );
    text(fb, small(), &meta, 110, 416);

    let items = ["1. Chapters", "2. Bookmarks", "3. Library", "4. Settings"];
    let mut ix = 110i16;
    for (i, item) in items.iter().enumerate() {
        let end = text(fb, body(), item, ix, 452);
        if i == 0 {
            hline(fb, ix, 462, end - ix);
        }
        ix = end + 36;
    }
}

// =====================================================================
// DIRECTION 5b — IMPRINT, DIRECT-MAP VARIANT ("marginalia")
// The X4 has a vertical 4-button column on the LEFT short side
// (top-to-bottom: Back, Confirm, Prev, Next) and two page keys on the
// long side. Here the left margin carries marginal notes aligned to
// the physical buttons: one label = one button, no cursor. While
// reading, the margin sleeps to four small ticks; any front button
// wakes it.
// =====================================================================

/// Vertical centers of the four left-bezel buttons on screen.
const KEY_YS: [i16; 4] = [120, 200, 280, 360];

/// Imprint B margin grammar: a typeset em-dash faces each physical
/// button — the same dash family as the folios and pagination — with
/// letterspaced small-caps labels: the device speaks in apparatus
/// type, never in the book's italic voice. The screen's one primary
/// action is bold. Key order is semantic: 1 primary, 2 elsewhere/exit,
/// 3-4 paired browse/secondary.
fn dash_key(fb: &mut Framebuffer, slot: usize, lines: &[&str], primary: bool) {
    let y = KEY_YS[slot];
    text(fb, body(), "\u{2014}", 10, y + 8);
    let font = if primary { small_bold() } else { small() };
    match lines {
        [single] => {
            ls_caps(fb, font, single, 40, y + 6, 2, BLACK);
        }
        [a, b] => {
            ls_caps(fb, font, a, 40, y - 5, 2, BLACK);
            ls_caps(fb, font, b, 40, y + 17, 2, BLACK);
        }
        _ => {}
    }
}

/// Home, re-set on the rail's grid: everything left-aligned to the
/// same column, apparatus bottom-right like every working screen.
fn imprintdm_home(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["library"], false);
    dash_key(fb, 1, &["continue"], true);
    dash_key(fb, 2, &["sync"], false);
    dash_key(fb, 3, &["settings"], false);

    let x = 210;
    text(fb, small_italic(), "now reading", x, 130);
    text(fb, display_font(), BOOK_TITLE, x, 196);
    text(fb, small_italic(), "by H. G. Wells", x, 234);
    let colophon = format!("Chapter {CHAPTER_LABEL} \u{00B7} Page 142 of 380 \u{00B7} 42 per cent");
    text(fb, small(), &colophon, x, 290);
    hline(fb, x, 326, 140);
    right(fb, small(), "82%", 740, 456);
}

fn imprintdm_library(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["home"], false);
    dash_key(fb, 1, &["open"], true);
    dash_key(fb, 2, &["previous"], false);
    dash_key(fb, 3, &["next"], false);
    dm_library_content(fb);
}

fn dm_library_content(fb: &mut Framebuffer) {
    let cx = 480;
    ls_caps_center(fb, small(), "Library", cx, 42, 5);
    hline(fb, cx - 160, 56, 320);

    let mut y = 118;
    for (i, (t, a, p)) in LIBRARY.iter().take(5).enumerate() {
        if i == 2 {
            text(fb, body(), "\u{2192}", 178, y);
        }
        let end_x = text(fb, body(), t, 210, y);
        let pct = if *p == 1000 {
            "read".to_string()
        } else if *p == 0 {
            "new".to_string()
        } else {
            format!("{}%", p / 10)
        };
        let pct_w = measure_text(body(), &pct) as i16;
        let mut dx = end_x + 16;
        while dx < 740 - pct_w - 14 {
            put(fb, dx, y - 2, BLACK);
            dx += 8;
        }
        right(fb, body(), &pct, 740, y);
        text(fb, small_italic(), a, 210, y + 22);
        y += 64;
    }

    center(fb, small(), "\u{2013} 1 of 2 \u{2013}", cx, 456);
    right(fb, small(), "82%", 740, 456);
}

// The B reading page is full bleed, matching the firmware reader's
// real bounds (x 8..792, footer strip at 466): the device bezel is
// the margin. No resident chrome except the firmware's own
// page-in-chapter counter.
fn dm_reading_page(fb: &mut Framebuffer) {
    let x = 8;
    let w = 784;
    let mut y = 28;
    y = para(fb, body(), TTM_P1, x, y, w, 30, true);
    y += 12;
    y = para(fb, body(), TTM_P2, x, y, w, 30, true);
    y += 12;
    para(fb, body(), TTM_P3, x, y, w, 30, true);
}

fn imprintdm_reading(fb: &mut Framebuffer) {
    dm_reading_page(fb);
    right(fb, small(), "12/38", 784, 477);
}

fn imprintdm_overlay(fb: &mut Framebuffer) {
    dm_reading_page(fb);

    // summoning CREATES the margin: an apparatus band over the page
    // bottom, and a key sheet slides in from the button edge
    rfill(fb, 0, 426, 800, 54, WHITE);
    hline(fb, 172, 440, 140);
    let meta = format!("{CHAPTER_LABEL} \u{00B7} {CHAPTER_NAME} \u{2014} page 12 of 38");
    text(fb, small(), &meta, 172, 466);
    right(fb, small(), "82%", 792, 466);

    rfill(fb, 0, 0, 160, 480, WHITE);
    vline(fb, 160, 0, 480);
    dash_key(fb, 0, &["close"], false);
    dash_key(fb, 1, &["contents"], true);
    dash_key(fb, 2, &["chapter", "back"], false);
    dash_key(fb, 3, &["chapter", "ahead"], false);
}

/// A settings/sync row in the language's index pattern: upright name,
/// dot leaders, italic value right-aligned — the colophon voice.
fn index_row(fb: &mut Framebuffer, name: &str, value: &str, y: i16, selected: bool) {
    if selected {
        text(fb, body(), "\u{2192}", 178, y);
    }
    let end_x = text(fb, body(), name, 210, y);
    let value_w = measure_text(body_italic(), value) as i16;
    let mut dx = end_x + 16;
    while dx < 740 - value_w - 14 {
        put(fb, dx, y - 2, BLACK);
        dx += 8;
    }
    let vx = 740 - value_w;
    text(fb, body_italic(), value, vx, y);
}

/// An unused key keeps its bare dash: the mark stays, the word goes.
fn dash_key_unused(fb: &mut Framebuffer, slot: usize) {
    text(fb, body(), "\u{2014}", 10, KEY_YS[slot] + 8);
}

fn imprintdm_settings(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["home"], false);
    dash_key(fb, 1, &["change"], true);
    dash_key(fb, 2, &["previous"], false);
    dash_key(fb, 3, &["next"], false);

    let cx = 480;
    ls_caps_center(fb, small(), "Settings", cx, 42, 5);
    hline(fb, cx - 160, 56, 320);

    let entries = [
        ("Refresh", "full every ten pages"),
        ("Orientation", "buttons left"),
        ("Type size", "22 pixel"),
        ("Sleep after", "ten minutes"),
        ("Wi-Fi", "off"),
    ];
    let mut y = 118;
    for (i, (name, value)) in entries.iter().enumerate() {
        index_row(fb, name, value, y, i == 0);
        y += 64;
    }

    right(fb, small(), "82%", 740, 456);
}

fn imprintdm_sync(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["begin"], true);
    dash_key(fb, 1, &["home"], false);
    dash_key_unused(fb, 2);
    dash_key_unused(fb, 3);

    let cx = 480;
    ls_caps_center(fb, small(), "Sync", cx, 42, 5);
    hline(fb, cx - 160, 56, 320);

    index_row(fb, "Network", "HOME-NET", 118, false);
    index_row(
        fb,
        "Last sync",
        "9 June \u{00B7} 14 books received",
        182,
        false,
    );
    index_row(fb, "On device", "41 books \u{00B7} 28.4 MB", 246, false);

    text(
        fb,
        small_italic(),
        "Begin fetches new books over Wi-Fi; the card's /books folder is read on wake.",
        210,
        330,
    );

    right(fb, small(), "82%", 740, 456);
}

/// Sleep: no key is listening, so by the language's own rule there is
/// no rail — the one ceremonial centered screen. No battery either;
/// a days-old panel image must not show stale numbers.
fn imprintdm_sleep(fb: &mut Framebuffer) {
    center(fb, display_font(), BOOK_TITLE, 400, 204);
    center(fb, small_italic(), "by H. G. Wells", 400, 242);
    hline(fb, 330, 282, 140);
    let mark = format!("Chapter {CHAPTER_LABEL} \u{00B7} page 12 of 38");
    center(fb, small(), &mark, 400, 314);
    center(fb, small(), "\u{00B7} asleep \u{00B7}", 400, 456);
}

/// Boot: the masthead that was evicted from home lives here, as the
/// printer's device on the half-title. Gone in four seconds.
fn imprintdm_boot(fb: &mut Framebuffer) {
    ls_caps_center(fb, small(), "Xteink X4", 400, 218, 6);
    rfill(fb, 240, 232, 320, 2, BLACK);
    hline(fb, 240, 238, 320);
    center(fb, small_italic(), "opening the library\u{2026}", 400, 292);
    center(
        fb,
        small(),
        "edition 0.4 \u{00B7} set in Literata",
        400,
        456,
    );
}

// =====================================================================
// MARGINALIA TREATMENT STUDY — five visual takes on the button-aligned
// left margin, all on the same home screen so only the margin varies.
// Labels: sync / continue (primary) / library / settings.
// =====================================================================

const HOME_KEYS: [&str; 4] = ["sync", "continue", "library", "settings"];
const HOME_PRIMARY: usize = 1;

fn imprintdm_home_content(fb: &mut Framebuffer) {
    let cx = 480;
    center(fb, small_italic(), "now reading", cx, 130);
    center(fb, display_font(), BOOK_TITLE, cx, 196);
    center(fb, small_italic(), "by H. G. Wells", cx, 234);
    let colophon = format!("Chapter {CHAPTER_LABEL} \u{00B7} Page 142 of 380 \u{00B7} 42 per cent");
    center(fb, small(), &colophon, cx, 290);
    hline(fb, cx - 70, 326, 140);
    center(fb, small(), "\u{00B7} 82% \u{00B7} 14:32 \u{00B7}", cx, 458);
}

/// v1 — solid black index tabs bleeding off the edge, white small caps,
/// chamfered like the thumb notches of a dictionary.
fn marginalia_thumb_tabs(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        let tab_w = 118i16;
        let tab_h = 36i16;
        rfill(fb, 0, y - tab_h / 2, tab_w, tab_h, BLACK);
        for step in 0..5i16 {
            rfill(fb, tab_w - 5 + step, y - tab_h / 2, 1, 5 - step, WHITE);
            rfill(
                fb,
                tab_w - 5 + step,
                y + tab_h / 2 - (5 - step),
                1,
                5 - step,
                WHITE,
            );
        }
        let font = if slot == HOME_PRIMARY {
            small_bold()
        } else {
            small()
        };
        ls_caps(fb, font, label, 14, y + 6, 1, WHITE);
    }
}

/// v2 — one continuous hairline down the edge with a tick at each key,
/// letterspaced small caps. Quiet and precise.
fn marginalia_registration(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    vline(fb, 14, KEY_YS[0] - 28, KEY_YS[3] - KEY_YS[0] + 56);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        rfill(fb, 8, y - 1, 13, 2, BLACK);
        let font = if slot == HOME_PRIMARY {
            small_bold()
        } else {
            small()
        };
        ls_caps(fb, font, label, 34, y + 6, 2, BLACK);
    }
}

/// v3 — each note set off by a left bracket, italic, like a printed
/// sidenote. Structured version of the original ticks.
fn marginalia_brackets(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        vline(fb, 16, y - 14, 28);
        hline(fb, 16, y - 14, 7);
        hline(fb, 16, y + 13, 7);
        let font = if slot == HOME_PRIMARY {
            small()
        } else {
            small_italic()
        };
        text(fb, font, label, 30, y + 6);
    }
}

/// v4 — a true ruled margin column: labels right-aligned against a
/// full-height hairline, ticks crossing at the key positions.
fn marginalia_ruled_column(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    vline(fb, 150, 84, 320);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        rfill(fb, 145, y - 1, 11, 2, BLACK);
        let font = if slot == HOME_PRIMARY {
            small()
        } else {
            small_italic()
        };
        let lx = 136 - measure_text(font, label) as i16;
        text(fb, font, label, lx, y + 6);
    }
}

/// v5 — a pilcrow marks each key, small caps labels. Pure printer's
/// furniture, no rules at all.
fn marginalia_pilcrows(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        text(fb, small(), "\u{00B6}", 12, y + 6);
        let font = if slot == HOME_PRIMARY {
            small_bold()
        } else {
            small()
        };
        ls_caps(fb, font, label, 34, y + 6, 1, BLACK);
    }
}

// =====================================================================
// BRACKET REFINEMENT STUDY — the bracket direction won on cleanliness
// but the execution felt off. Two theories tested: (a) drawn rectangle
// linework reads as CAD, the furniture should be TYPESET Literata
// glyphs; (b) the 16px italic labels were too timid.
// =====================================================================

/// v1 — the drawn bracket, but with conviction: 2px stem, longer arms,
/// letterspaced small caps instead of timid italics.
fn bracket_refined_drawn(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        rfill(fb, 14, y - 18, 2, 36, BLACK);
        hline(fb, 16, y - 18, 9);
        hline(fb, 16, y + 17, 9);
        let font = if slot == HOME_PRIMARY {
            small_bold()
        } else {
            small()
        };
        ls_caps(fb, font, label, 32, y + 6, 2, BLACK);
    }
}

/// v2 — the bracket is a real Literata "[" at 30px: stroke modulation,
/// real serifs on the arms. Small caps labels.
fn bracket_typeset_caps(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        text(fb, title(), "[", 12, y + 11);
        let font = if slot == HOME_PRIMARY {
            small_bold()
        } else {
            small()
        };
        ls_caps(fb, font, label, 32, y + 6, 2, BLACK);
    }
}

/// v3 — typeset bracket with full-size body italic labels: the margin
/// speaks at the same size as the book itself.
fn bracket_typeset_italic(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        text(fb, title(), "[", 12, y + 11);
        let font = if slot == HOME_PRIMARY {
            body()
        } else {
            body_italic()
        };
        text(fb, font, label, 32, y + 8);
    }
}

/// v4 — no bracket at all: a typeset em-dash leads each note, the way
/// printed indexes mark entries. Body italic labels.
fn bracket_emdash(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        text(fb, body(), "\u{2014}", 10, y + 8);
        let font = if slot == HOME_PRIMARY {
            body()
        } else {
            body_italic()
        };
        text(fb, font, label, 44, y + 8);
    }
}

/// v5 — contents-page voice: lowercase roman numerals, body labels,
/// position alone maps numeral to button.
fn bracket_numerals(fb: &mut Framebuffer) {
    imprintdm_home_content(fb);
    let numerals = ["i.", "ii.", "iii.", "iv."];
    for (slot, label) in HOME_KEYS.iter().enumerate() {
        let y = KEY_YS[slot];
        right(fb, body(), numerals[slot], 38, y + 8);
        let font = if slot == HOME_PRIMARY {
            body()
        } else {
            body_italic()
        };
        text(fb, font, label, 50, y + 8);
    }
}

// =====================================================================
// COUNTER-PROPOSAL STUDY — the two alternatives still in play against
// the canonical em-dash + body italic / left-grid home set.
// =====================================================================

/// Em-dash + lowercase body-italic labels (the superseded treatment),
/// on the library screen, kept for comparison.
fn altstudy_library_italic(fb: &mut Framebuffer) {
    let labels = ["open", "home", "previous", "next"];
    for (slot, label) in labels.iter().enumerate() {
        let y = KEY_YS[slot];
        text(fb, body(), "\u{2014}", 10, y + 8);
        let font = if slot == 0 { body() } else { body_italic() };
        text(fb, font, label, 44, y + 8);
    }
    dm_library_content(fb);
}

/// The centered title-page home, kept in the running in em-dash form.
fn altstudy_home_centered(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["continue"], true);
    dash_key(fb, 1, &["library"], false);
    dash_key(fb, 2, &["sync"], false);
    dash_key(fb, 3, &["settings"], false);
    imprintdm_home_content(fb);
}

// =====================================================================
// V2 HOME ITERATION SHEET — variations on the as-flashed home, attacking
// two flagged problems: the "Chapter 1 · 42 per cent" colophon (verbose,
// mixed units) and the lowercase italic "now reading" / "by" labels.
// The key rail is the shipped one: library / CONTINUE / sync / settings.
// =====================================================================

fn roman(n: usize) -> &'static str {
    const NUMERALS: [&str; 20] = [
        "I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII", "XIII", "XIV",
        "XV", "XVI", "XVII", "XVIII", "XIX", "XX",
    ];
    NUMERALS.get(n.saturating_sub(1)).copied().unwrap_or("—")
}

fn iter_rail(fb: &mut Framebuffer) {
    dash_key(fb, 0, &["library"], false);
    dash_key(fb, 1, &["continue"], true);
    dash_key(fb, 2, &["sync"], false);
    dash_key(fb, 3, &["settings"], false);
}

const ITER_X: i16 = 210;

/// 1 — the as-flashed baseline, for honest comparison.
fn iter_baseline(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, small_italic(), "now reading", ITER_X, 130);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 196);
    text(fb, small_italic(), "by H. G. Wells", ITER_X, 234);
    text(fb, small(), "Chapter 7 \u{00B7} 42 per cent", ITER_X, 290);
    hline(fb, ITER_X, 326, 140);
    right(fb, small(), "82%", 740, 456);
}

/// 2 — label becomes apparatus caps; author plain; colophon in pages.
fn iter_caps_label(fb: &mut Framebuffer) {
    iter_rail(fb);
    ls_caps(fb, small(), "Now Reading", ITER_X, 130, 3, BLACK);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 196);
    text(fb, small_italic(), "H. G. Wells", ITER_X, 234);
    let colophon = format!("Chapter {} \u{00B7} page 142 of 380", roman(7));
    text(fb, small(), &colophon, ITER_X, 290);
    hline(fb, ITER_X, 326, 140);
    right(fb, small(), "82%", 740, 456);
}

/// 3 — no label at all; the rule becomes the progress hairline.
fn iter_no_label(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 170);
    text(fb, small_italic(), "H. G. Wells", ITER_X, 208);
    progress_hairline(fb, ITER_X, 268, 240, 420);
    text(fb, small(), "page 142 of 380", ITER_X, 300);
    right(fb, small(), "82%", 740, 456);
}

/// 4 — keep the italic label, fix only the colophon: pages alone.
fn iter_pages_only(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, small_italic(), "now reading", ITER_X, 130);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 196);
    text(fb, small_italic(), "H. G. Wells", ITER_X, 234);
    text(fb, small(), "page 142 of 380", ITER_X, 290);
    hline(fb, ITER_X, 326, 140);
    right(fb, small(), "82%", 740, 456);
}

/// 5 — classic title page: author in letterspaced caps, progress rule.
fn iter_author_caps(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 180);
    ls_caps(fb, small(), "H. G. Wells", ITER_X, 222, 3, BLACK);
    progress_hairline(fb, ITER_X, 280, 240, 420);
    let colophon = format!("{} \u{00B7} page 142 of 380", roman(7));
    text(fb, small(), &colophon, ITER_X, 312);
    right(fb, small(), "82%", 740, 456);
}

/// 6b — the corrected measure: endpoints are the book (1..380); the
/// current page is a marked point ON the scale, not a range end.
fn iter_measure_fixed(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 180);
    text(fb, small_italic(), "H. G. Wells", ITER_X, 218);

    let left = "1";
    let right_label = "380";
    let left_w = measure_text(small(), left) as i16;
    let rule_x = ITER_X + left_w + 12;
    let rule_w = 280i16;
    let rule_y = 290i16;
    text(fb, small(), left, ITER_X, rule_y + 6);
    hline(fb, rule_x, rule_y, rule_w);
    // heavier line up to the marker, then the you-are-here tick
    let pos = rule_x + (rule_w as i32 * 420 / 1000) as i16;
    rfill(fb, rule_x, rule_y - 1, pos - rule_x, 3, BLACK);
    rfill(fb, pos, rule_y - 6, 2, 13, BLACK);
    text(fb, small(), right_label, rule_x + rule_w + 12, rule_y + 6);
    // the current page floats over its mark
    let page = "142";
    let page_w = measure_text(small(), page) as i16;
    text(fb, small(), page, pos - page_w / 2, rule_y - 14);

    right(fb, small(), "82%", 740, 456);
}

/// 8 — the chosen direction: iter-5's title page, with the chapter
/// NAME in the book's italic voice instead of a roman numeral.
fn iter_chapter_name(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 180);
    ls_caps(fb, small(), "H. G. Wells", ITER_X, 222, 3, BLACK);
    progress_hairline(fb, ITER_X, 280, 240, 420);
    let end = text(fb, small_italic(), CHAPTER_NAME, ITER_X, 312);
    text(fb, small(), " \u{00B7} page 142 of 380", end, 312);
    right(fb, small(), "82%", 740, 456);
}

/// 6 — the measure: page count flanks the progress rule like a scale.
fn iter_measure(fb: &mut Framebuffer) {
    iter_rail(fb);
    text(fb, display_font(), BOOK_TITLE, ITER_X, 180);
    text(fb, small_italic(), "H. G. Wells", ITER_X, 218);
    let left = "142";
    let right_label = "380";
    let left_w = measure_text(small(), left) as i16;
    text(fb, small(), left, ITER_X, 288);
    let rule_x = ITER_X + left_w + 12;
    let rule_w = 240;
    progress_hairline(fb, rule_x, 282, rule_w, 420);
    text(fb, small(), right_label, rule_x + rule_w + 12, 288);
    right(fb, small(), "82%", 740, 456);
}

// ---------------------------------------------------------------- gallery

fn write_gallery_html(
    dir: &Path,
    screens: &[(&str, &str, fn(&mut Framebuffer))],
) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(dir.join("index.html"))?);
    writeln!(
        file,
        "<!doctype html><meta charset=utf-8><title>X4 design directions</title>\
         <style>body{{background:#15151a;color:#ddd;font:14px/1.5 -apple-system,sans-serif;\
         margin:40px}}h1{{font-weight:600}}h2{{margin:48px 0 4px;font-weight:600}}\
         p.blurb{{margin:0 0 16px;color:#999;max-width:70ch}}\
         .row{{display:flex;gap:16px;flex-wrap:wrap}}figure{{margin:0}}\
         img{{width:380px;image-rendering:pixelated;border:1px solid #333;display:block}}\
         figcaption{{color:#888;font-size:12px;padding:4px 0}}</style>"
    )?;
    writeln!(file, "<h1>Xteink X4 \u{2014} four design directions</h1>")?;
    let sections = [
        (
            "zen",
            "Zen Minimal \u{2014} \u{201C}Still Water\u{201D}",
            "One thing per screen. No boxes, no chrome; type floats in whitespace. \
             Chrome is summoned, never resident.",
        ),
        (
            "shelf",
            "The Shelf \u{2014} book-forward",
            "Covers are the heroes. The library is a place you browse, the home screen \
             is the book you left open.",
        ),
        (
            "folio",
            "Folio \u{2014} editorial print",
            "The UI is typeset like a fine book: hairline rules, letterspaced small caps, \
             dot leaders, folios and footnotes.",
        ),
        (
            "cockpit",
            "Cockpit \u{2014} instrument panel",
            "Honest hardware. Status bars, gauges, tabular data, terminal type, \
             reading stats front and centre.",
        ),
        (
            "imprint-",
            "Imprint A \u{2014} cursor model",
            "Folio's typographic logic everywhere, Zen's restraint inside the book. \
             Layout follows the content; Prev/Next move an underline cursor through \
             whatever is on screen, Confirm activates, Back dismisses.",
        ),
        (
            "imprintdm-",
            "Imprint B \u{2014} direct map (marginalia)",
            "Same language, hardware-driven layout: the X4's four left-bezel buttons \
             get small-caps notes aligned beside them \u{2014} one label, one button, no \
             cursor. Reading is full-bleed text (the bezel is the margin); any front \
             button summons the key sheet and apparatus band over the page. Page keys \
             on the long side always turn pages.",
        ),
        (
            "marginalia-",
            "Marginalia treatment study",
            "Five visual takes on the button-aligned margin, identical home content. \
             Pick the one the rest of Imprint B should be set in.",
        ),
        (
            "bracket-",
            "Bracket refinement study",
            "The bracket direction, executed five ways: drawn linework vs typeset \
             Literata furniture, small caps vs full-size body labels.",
        ),
        (
            "altstudy-",
            "Counter-proposals",
            "Against the canonical small-caps set: the superseded body-italic \
             labels (library), and the centered title-page home.",
        ),
        (
            "iter-",
            "V2 home iterations",
            "Variations on the as-flashed home, attacking the colophon \
             (Chapter N \u{00B7} per cent) and the lowercase italic labels. \
             Key rail matches the device: library / CONTINUE / sync / settings.",
        ),
    ];
    for (prefix, heading, blurb) in sections {
        writeln!(
            file,
            "<h2>{heading}</h2><p class=blurb>{blurb}</p><div class=row>"
        )?;
        for (name, label, _) in screens.iter().filter(|(n, _, _)| n.starts_with(prefix)) {
            writeln!(
                file,
                "<figure><img src=\"{name}.png\"><figcaption>{label}</figcaption></figure>"
            )?;
        }
        writeln!(file, "</div>")?;
    }
    Ok(())
}
