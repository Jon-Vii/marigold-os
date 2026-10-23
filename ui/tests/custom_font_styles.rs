//! Draw stored reading lines through the firmware's SD custom-font page body
//! (`fw/src/custom_font.rs`) on the host. A synthetic pack gives each style
//! its own glyph and advance, so a run drawn or measured in the wrong face
//! shows in the pixels; the expected page is drawn here from the runs alone.
// Whole firmware modules come in: most of their items go unused here, and
// their `bench-serial` feature is not one of this crate's.
#![allow(dead_code, unused_imports, unexpected_cfgs)]
#[path = "../../fw/src/custom_font.rs"]
mod custom_font;
#[path = "support/fat.rs"]
mod fat;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;

use display::fb::Framebuffer;
use display::font::{
    style_marker_code, FontFamily, FontSize, FontStyle, TypeSettings, STYLE_MARKER,
};
use embedded_sdmmc::{Mode, VolumeIdx, VolumeManager};
use proto::cache::{PageRecord, CACHE_ROOT_DIR};
use proto::font_pack::{
    FontPackFaceRecord, FONT_PACK_DIR, FONT_PACK_FILE, FONT_PACK_METRIC_BYTES,
    FONT_PACK_SIZE_MEDIUM,
};
use proto::text::{TextAlign, TextRole};
use reader_store::ReaderStore;
use ui::reading::ReadingBlocks;

const STYLES: [FontStyle; 4] = [
    FontStyle::Regular,
    FontStyle::Italic,
    FontStyle::Bold,
    FontStyle::BoldItalic,
];
/// Printable ASCII, the start of every face's metric table.
const ASCII_GLYPHS: usize = 95;
const GLYPH_HEIGHT: usize = 8;
const GLYPH_WIDTH: u8 = 4;

/// Pack face index of a style at the Normal weight.
fn face_of(style: FontStyle) -> usize {
    STYLES.iter().position(|s| *s == style).unwrap()
}

/// Each face advances its glyphs and spaces by its own width.
fn advance(style: FontStyle, ch: char) -> i16 {
    let face = face_of(style) as i16;
    if ch == ' ' {
        3 + face
    } else {
        6 + face
    }
}

/// One row pattern per face; every glyph of a face draws the same 4x8 cell.
fn glyph_row(face: usize, row: usize) -> u8 {
    match face {
        0 => 0xF0,
        1 => [0xA0, 0x50][row % 2],
        2 => [0xF0, 0x00][row % 2],
        _ => 0x90,
    }
}

/// A pack with Regular, Italic, Bold, and BoldItalic at the medium size:
/// the face records for the store and the file bytes. Characters outside
/// ASCII fall back to the face's `?`, which draws the same cell.
fn synthetic_pack() -> ([FontPackFaceRecord; 4], Vec<u8>) {
    let metrics_bytes = ASCII_GLYPHS * FONT_PACK_METRIC_BYTES;
    let bitmaps_at = 4 * metrics_bytes;
    let mut bytes = vec![0u8; bitmaps_at + 4 * GLYPH_HEIGHT];
    let mut faces = [FontPackFaceRecord::EMPTY; 4];
    for (face, style) in STYLES.iter().enumerate() {
        for index in 0..ASCII_GLYPHS {
            let ch = char::from(0x20 + index as u8);
            let (len, width, height) = if ch == ' ' {
                (0u16, 0u8, 0u8)
            } else {
                (GLYPH_HEIGHT as u16, GLYPH_WIDTH, GLYPH_HEIGHT as u8)
            };
            let at = face * metrics_bytes + index * FONT_PACK_METRIC_BYTES;
            let record = &mut bytes[at..at + FONT_PACK_METRIC_BYTES];
            record[0..4].copy_from_slice(&0u32.to_le_bytes());
            record[4..6].copy_from_slice(&len.to_le_bytes());
            record[6] = width;
            record[7] = height;
            record[8] = 0;
            record[9] = (-(GLYPH_HEIGHT as i8)) as u8;
            record[10..12].copy_from_slice(&((advance(*style, ch) as u16) << 4).to_le_bytes());
        }
        for row in 0..GLYPH_HEIGHT {
            bytes[bitmaps_at + face * GLYPH_HEIGHT + row] = glyph_row(face, row);
        }
        faces[face] = FontPackFaceRecord {
            size_px: FONT_PACK_SIZE_MEDIUM,
            style: face as u8,
            line_height: 26,
            baseline: 20,
            flags: 0,
            metrics_offset: (face * metrics_bytes) as u32,
            metric_count: ASCII_GLYPHS as u32,
            bitmap_offset: (bitmaps_at + face * GLYPH_HEIGHT) as u32,
            bitmap_len: GLYPH_HEIGHT as u32,
            kerning_offset: 0,
            kerning_count: 0,
        };
    }
    (faces, bytes)
}

/// One stored line, given as its true style runs. A run's spaces end it, as
/// the line builder writes them.
struct Line {
    runs: &'static [(FontStyle, &'static str)],
    align: TextAlign,
    paragraph_end: bool,
}

const LINES: &[Line] = &[
    // A plain lead-in, then italic, opening a paragraph (justified).
    Line {
        runs: &[
            (
                FontStyle::Regular,
                "At last the Mouse, who seemed to be a person of some authority among them, \
                 called out to them all ",
            ),
            (FontStyle::Italic, "Sit down, all of you, "),
            (FontStyle::Regular, "and"),
        ],
        align: TextAlign::Justify,
        paragraph_end: false,
    },
    // An italic run continued from the line before.
    Line {
        runs: &[
            (FontStyle::Italic, "listen to me! "),
            (
                FontStyle::Regular,
                "I will soon make you dry enough! They all sat down at once, in a large ring, \
                 with the Mouse in the middle.",
            ),
        ],
        align: TextAlign::Justify,
        paragraph_end: false,
    },
    // A plain lead-in, then bold.
    Line {
        runs: &[
            (FontStyle::Regular, "Alice kept her eyes "),
            (FontStyle::Bold, "anxiously fixed on it, "),
            (
                FontStyle::Regular,
                "for she felt sure she would catch a bad cold if she did not get dry very soon,",
            ),
        ],
        align: TextAlign::Justify,
        paragraph_end: false,
    },
    // A plain lead-in, then bold italic, ending the paragraph.
    Line {
        runs: &[
            (FontStyle::Regular, "and she would catch a "),
            (FontStyle::BoldItalic, "bad cold "),
            (FontStyle::Regular, "after all."),
        ],
        align: TextAlign::Justify,
        paragraph_end: true,
    },
    // Centred, plain lead-in, then italic.
    Line {
        runs: &[
            (FontStyle::Regular, "The "),
            (FontStyle::Italic, "Mouse's "),
            (FontStyle::Regular, "Long and Sad Tale"),
        ],
        align: TextAlign::Center,
        paragraph_end: true,
    },
    // Only emphasis, opening a paragraph.
    Line {
        runs: &[(
            FontStyle::Italic,
            "Ahem! said the Mouse with an important air, are you all ready? This is the \
             driest thing I know. Silence",
        )],
        align: TextAlign::Justify,
        paragraph_end: false,
    },
    // A plain lead-in, then italic, at natural spacing (paragraph end).
    Line {
        runs: &[
            (FontStyle::Regular, "all round, if you "),
            (FontStyle::Italic, "please!"),
        ],
        align: TextAlign::Justify,
        paragraph_end: true,
    },
];

fn push_marker(out: &mut String, style: FontStyle) {
    out.push(STYLE_MARKER);
    out.push(style_marker_code(style));
}

/// The line as the builder stores it: it opens in Regular and marks a run
/// only where the style changes.
fn builder_text(runs: &[(FontStyle, &str)]) -> String {
    let mut out = String::new();
    let mut current = FontStyle::Regular;
    for (style, text) in runs {
        if *style != current {
            push_marker(&mut out, *style);
            current = *style;
        }
        out.push_str(text);
    }
    out
}

/// The block style v0.6.1's builder stored: the line's first marker.
fn v061_block_style(text: &str) -> FontStyle {
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == STYLE_MARKER {
            return chars
                .next()
                .and_then(display::font::style_from_marker_code)
                .unwrap_or(FontStyle::Regular);
        }
    }
    FontStyle::Regular
}

fn custom_settings() -> TypeSettings {
    TypeSettings {
        family: FontFamily::Custom,
        size: FontSize::Medium,
        ..TypeSettings::DEFAULT
    }
}

/// A store holding `LINES` as one page, each line stored as `stored` makes
/// it from the builder's text: the text to store and its block style.
fn store_lines(stored: impl Fn(&str) -> (String, FontStyle)) -> Box<ReaderStore> {
    let (faces, _) = synthetic_pack();
    let mut store = Box::new(ReaderStore::new());
    store.set_layout(custom_settings(), false);
    store.set_custom_font(Some("Synthetic"), 0x5157_4c45, &faces);
    for line in LINES {
        let (text, style) = stored(&builder_text(line.runs));
        assert!(store.push_line_block(
            &text,
            style,
            TextRole::Body,
            line.align,
            line.paragraph_end,
            0,
        ));
    }
    reader_layout::rebuild_page_index(&mut store);
    assert_eq!(store.pages[0].block_count as usize, LINES.len());
    store
}

/// Draw the store's first page through the custom-font page body, reading
/// the pack from an in-memory card.
fn draw_custom_page(store: &ReaderStore) -> Box<Framebuffer> {
    let (_, pack) = synthetic_pack();
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    root.make_dir_in_dir(CACHE_ROOT_DIR).unwrap();
    let xteink = root.open_dir(CACHE_ROOT_DIR).unwrap();
    xteink.make_dir_in_dir(FONT_PACK_DIR).unwrap();
    let fonts = xteink.open_dir(FONT_PACK_DIR).unwrap();
    let file = fonts
        .open_file_in_dir(FONT_PACK_FILE, Mode::ReadWriteCreate)
        .unwrap();
    file.write(&pack).unwrap();
    file.close().unwrap();
    drop(fonts);
    drop(xteink);

    let mut fb = Box::new(Framebuffer::new());
    let mut metrics = Box::new(custom_font::MetricCache::new());
    assert!(custom_font::draw_reading_page_body(
        &root,
        &mut fb,
        store,
        &mut metrics,
        store.pages[0],
        |_, _| {},
    ));
    fb
}

/// The page as it should look, drawn from the runs: each character in its
/// run's face at that face's advance, with the justified lines' leftover
/// width spread over their gaps by `ui::reading`'s rule, the opening run's
/// space setting the stretch limit.
fn expected_page(store: &ReaderStore) -> (Box<Framebuffer>, usize) {
    let page_box = store.page_box();
    let mut baselines = Vec::new();
    ui::reading::for_each_drawable_block(store, store.pages[0], |block| {
        baselines.push((block.y, block.indent));
        true
    });
    let mut fb = Box::new(Framebuffer::new());
    let mut justified = 0;
    for (line, (baseline, indent)) in LINES.iter().zip(baselines) {
        let chars: Vec<(FontStyle, char)> = line
            .runs
            .iter()
            .flat_map(|(style, text)| text.chars().map(move |ch| (*style, ch)))
            .collect();
        let natural: i16 = chars.iter().map(|(style, ch)| advance(*style, *ch)).sum();
        let gaps = chars.iter().filter(|(_, ch)| *ch == ' ').count();
        let mut x = match line.align {
            TextAlign::Center => {
                let width = natural.min(page_box.right - page_box.left);
                ((page_box.left + page_box.right - width) / 2).max(page_box.left)
            }
            TextAlign::Left | TextAlign::Justify => page_box.x_for(TextRole::Body) + indent,
        };
        let extra = (line.align == TextAlign::Justify && !line.paragraph_end)
            .then(|| {
                let space = advance(line.runs[0].0, ' ');
                ui::reading::justify_extra(x, natural, page_box.right, gaps, space)
            })
            .flatten();
        justified += usize::from(extra.is_some());
        let (per_gap, mut remainder) =
            extra.map_or((0, 0), |extra| (extra / gaps as i16, extra % gaps as i16));
        for (style, ch) in chars {
            if ch == ' ' {
                x += per_gap;
                if remainder > 0 {
                    x += 1;
                    remainder -= 1;
                }
            } else {
                let face = face_of(style);
                for row in 0..GLYPH_HEIGHT {
                    let bits = glyph_row(face, row);
                    for column in 0..GLYPH_WIDTH as usize {
                        if bits & (0x80 >> column) != 0 {
                            let px = x as usize + column;
                            let py = (baseline as usize) - GLYPH_HEIGHT + row;
                            fb.set_pixel(px, py, false);
                        }
                    }
                }
            }
            x += advance(style, ch);
        }
    }
    (fb, justified)
}

/// Lines stored as caches before the fix stored them (block style = first
/// marker) and as the builder stores them now both draw each run in its own
/// face, measured and justified by the true styles.
#[test]
fn custom_font_lines_draw_every_run_in_its_own_style() {
    for (label, store) in [
        (
            "first marker",
            store_lines(|text| (text.into(), v061_block_style(text))),
        ),
        (
            "opening style",
            store_lines(|text| {
                let style = ui::reading::line_opening_style(text, FontStyle::Regular);
                (text.into(), style)
            }),
        ),
    ] {
        let (want, justified) = expected_page(&store);
        assert_eq!(justified, 4, "{label}: the mid-paragraph lines stretch");
        let got = draw_custom_page(&store);
        let differing = (0..got.frame_height())
            .flat_map(|y| (0..got.frame_width()).map(move |x| (x, y)))
            .filter(|&(x, y)| got.pixel(x, y) != want.pixel(x, y))
            .count();
        assert_eq!(differing, 0, "{label}: {differing} pixels differ");
    }
}

/// The fixture would catch the bug: starting each line in its stored first
/// marker, as the drawer used to, draws a different page.
#[test]
fn custom_font_old_start_style_differs() {
    let fixed = draw_custom_page(&store_lines(|text| (text.into(), v061_block_style(text))));
    // A leading marker makes a line open in its stored style whatever the
    // drawer derives, as the old drawer did.
    let old = draw_custom_page(&store_lines(|text| {
        let style = v061_block_style(text);
        let mut prefixed = String::new();
        push_marker(&mut prefixed, style);
        prefixed.push_str(text);
        (prefixed, style)
    }));
    assert!(
        old.bytes() != fixed.bytes(),
        "the old start style should draw differently"
    );
}
