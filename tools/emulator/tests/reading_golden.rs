//! Golden-frame coverage for the shared reading surface: paginate and draw
//! synthetic cached blocks through `ui::reading` — the exact code firmware
//! uses for SD reading pages — and compare against checked-in frames.
//!
//! Golden images are pinned per-board (X4's 800x480 and X3's 792x528 each
//! get their own files, see `golden_path`); pixel comparisons run on both.
//!
//! Regenerate after intentional typography changes with:
//! `REGEN_READING_GOLDEN=1 cargo test --manifest-path tools/emulator/Cargo.toml --target <host> --test reading_golden`
//! (repeat with `--features device-x3` to refresh the X3 frames too).
use std::path::{Path, PathBuf};

use display::fb::Framebuffer;
use display::font::{
    style_marker_code, FontFamily, FontSize, FontStyle, FontWeight, LineSpacing, TypeSettings,
    STYLE_MARKER,
};
use proto::cache::BlockRecord;
use proto::text::{TextAlign, TextRole};
use ui::reading::{draw_reading_page_body, draw_reading_page_counter, page_record_at};
use ui::reading::{
    block_first_line_indent, body_font, paginate_block_pages, wrapped_line_count, PageBox,
    ReadingBlocks,
};

struct FixtureBlock {
    record: BlockRecord,
    text: String,
    style: FontStyle,
    page_break_before: bool,
    paragraph_end: bool,
}

struct FixtureBlocks {
    blocks: Vec<FixtureBlock>,
    settings: TypeSettings,
    portrait: bool,
}

impl ReadingBlocks for FixtureBlocks {
    fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn block(&self, index: usize) -> Option<BlockRecord> {
        self.blocks.get(index).map(|block| block.record)
    }

    fn block_text(&self, index: usize) -> &str {
        self.blocks
            .get(index)
            .map(|block| block.text.as_str())
            .unwrap_or("")
    }

    fn block_style(&self, index: usize) -> FontStyle {
        self.blocks
            .get(index)
            .map(|block| block.style)
            .unwrap_or(FontStyle::Regular)
    }

    fn page_break_before(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .map(|block| block.page_break_before)
            .unwrap_or(false)
    }

    fn paragraph_end(&self, index: usize) -> bool {
        self.blocks
            .get(index)
            .map(|block| block.paragraph_end)
            .unwrap_or(true)
    }

    fn type_settings(&self) -> TypeSettings {
        self.settings
    }

    fn page_box(&self) -> PageBox {
        PageBox::for_portrait(self.portrait)
    }
}

fn record(role: TextRole, align: TextAlign, line_count: u8) -> BlockRecord {
    BlockRecord {
        text_offset: 0,
        text_len: 0,
        line_count,
        role,
        style: proto::text::FontStyle::Regular,
        align,
    }
}

/// Build cached-text style runs the way the firmware sink does: a style
/// marker before each run of words.
fn styled(runs: &[(FontStyle, &str)]) -> String {
    let mut out = String::new();
    for (index, (style, words)) in runs.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push(STYLE_MARKER);
        out.push(style_marker_code(*style));
        out.push_str(words);
    }
    out
}

fn fixture(settings: TypeSettings) -> FixtureBlocks {
    let mut blocks = Vec::new();
    blocks.push(FixtureBlock {
        record: record(TextRole::Heading1, TextAlign::Center, 1),
        text: styled(&[(FontStyle::Bold, "Chapter One")]),
        style: FontStyle::Bold,
        page_break_before: false,
        paragraph_end: true,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::Body, TextAlign::Justify, 4),
        text: "It was the best of times, it was the worst of times, it was the age of \
               wisdom, it was the age of foolishness, it was the epoch of belief, it was \
               the epoch of incredulity, it was the season of Light, it was the season of \
               Darkness, it was the spring of hope, it was the winter of despair."
            .into(),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end: true,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::Body, TextAlign::Justify, 1),
        text: styled(&[
            (FontStyle::Regular, "Mixed runs:"),
            (FontStyle::Italic, "slanted words"),
            (FontStyle::Bold, "heavy words"),
            (FontStyle::Regular, "and plain again."),
        ]),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end: true,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::BlockQuote, TextAlign::Left, 2),
        text: "A quoted aside, indented from the left margin and wrapped across more \
               than a single line to exercise the blockquote geometry."
            .into(),
        style: FontStyle::Italic,
        page_break_before: false,
        paragraph_end: true,
    });
    for paragraph in 0..6 {
        blocks.push(FixtureBlock {
            record: record(TextRole::Body, TextAlign::Justify, 3),
            text: format!(
                "Filler paragraph number {paragraph} pads the page so pagination crosses \
                 a boundary; the quick brown fox jumps over the lazy dog while accented \
                 caf\u{e9} text and em\u{2014}dashes keep the glyph set honest."
            ),
            style: FontStyle::Regular,
            page_break_before: false,
            paragraph_end: true,
        });
    }
    blocks.push(FixtureBlock {
        record: record(TextRole::Heading2, TextAlign::Center, 1),
        text: styled(&[(FontStyle::Bold, "Forced Second Page")]),
        style: FontStyle::Bold,
        page_break_before: true,
        paragraph_end: true,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::Body, TextAlign::Center, 1),
        text: styled(&[(FontStyle::Regular, "* * *")]),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end: true,
    });
    FixtureBlocks {
        blocks,
        settings,
        portrait: false,
    }
}

/// The same fixture laid into the portrait page box. Cached line counts
/// are wrap products, so they re-wrap under the portrait widths exactly as
/// a device cache rebuild would.
fn portrait_fixture() -> FixtureBlocks {
    let mut source = fixture(TypeSettings::DEFAULT);
    source.portrait = true;
    let counts: Vec<u8> = (0..source.blocks.len())
        .map(|index| {
            // The device sink emits styled runs only as pre-wrapped
            // single-line blocks, so marker-bearing text stays one line;
            // wrapping it would paint the markers as glyphs.
            if source.blocks[index].text.contains(STYLE_MARKER) {
                return source.blocks[index].record.line_count;
            }
            let record = source.blocks[index].record;
            let font = body_font(source.settings, source.blocks[index].style);
            let page_box = ReadingBlocks::page_box(&source);
            let max_width = page_box.right
                - if record.align == TextAlign::Center {
                    page_box.left
                } else {
                    page_box.x_for(record.role)
                };
            let indent = block_first_line_indent(&source, index);
            wrapped_line_count(font, &source.blocks[index].text, max_width, indent)
                .max(1)
                .min(u8::MAX as u16) as u8
        })
        .collect();
    for (block, count) in source.blocks.iter_mut().zip(counts) {
        block.record.line_count = count;
    }
    source
}

fn encode_png(fb: &Framebuffer) -> Vec<u8> {
    // Same mapping as the emulator's render::encode_png so frames are
    // directly comparable with the scenario goldens.
    let mut bytes = Vec::new();
    {
        let mut encoder =
            png::Encoder::new(&mut bytes, display::WIDTH as u32, display::HEIGHT as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        let mut data = Vec::with_capacity(display::WIDTH * display::HEIGHT);
        for y in 0..display::HEIGHT {
            for x in 0..display::WIDTH {
                data.push(if fb.native_pixel(x, y) { 0xEE } else { 0x18 });
            }
        }
        writer.write_image_data(&data).expect("png data");
    }
    bytes
}

fn golden_path(name: &str) -> PathBuf {
    let suffix = if cfg!(feature = "device-x3") { "-x3" } else { "" };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(format!("{name}{suffix}.png"))
}

fn assert_page_matches_golden(source: &FixtureBlocks, page_index: usize, name: &str) {
    let page = page_record_at(source, page_index);
    assert!(page.block_count > 0, "page {page_index} should hold blocks");
    let mut fb = Framebuffer::new();
    draw_reading_page_body(&mut fb, source, page);
    let actual = encode_png(&fb);
    let path = golden_path(name);
    if std::env::var("REGEN_READING_GOLDEN").is_ok() {
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden {} ({err}); run with REGEN_READING_GOLDEN=1 to create",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "reading page {page_index} diverged from {}",
        path.display()
    );
}

#[test]
fn reading_page_bodies_match_goldens() {
    let source = fixture(TypeSettings::DEFAULT);
    let pages = paginate_block_pages(&source);
    assert!(pages >= 2, "fixture should span at least two pages, got {pages}");

    for page_index in 0..2 {
        assert_page_matches_golden(&source, page_index, &format!("reading-page-{page_index}"));
    }
}

/// Pin the complete reading surface, including the page-in-chapter counter.
/// The body-only goldens above isolate pagination and typography; this frame
/// catches footer font, inset, and panel-relative baseline regressions.
#[test]
fn full_reading_surface_matches_golden() {
    let source = fixture(TypeSettings::DEFAULT);
    let page = page_record_at(&source, 0);
    let mut fb = Framebuffer::new();
    draw_reading_page_body(&mut fb, &source, page);
    draw_reading_page_counter(&mut fb, "1/2");

    let actual = encode_png(&fb);
    let path = golden_path("reading-surface-0");
    if std::env::var("REGEN_READING_GOLDEN").is_ok() {
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden {} ({err}); run with REGEN_READING_GOLDEN=1 to create",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "reading surface diverged from {}",
        path.display()
    );
}

/// The same blocks at the large size with relaxed leading: fewer lines fit
/// a page, so the fixture must paginate onto more pages than the default,
/// and the first page's frame is pinned.
#[test]
fn reading_page_bodies_match_goldens_large_relaxed() {
    let source = fixture(TypeSettings {
        size: FontSize::Large,
        spacing: LineSpacing::Relaxed,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
    });
    let default_pages =
        paginate_block_pages(&fixture(TypeSettings::DEFAULT));
    let pages = paginate_block_pages(&source);
    assert!(
        pages > default_pages,
        "large/relaxed must need more pages ({pages}) than default ({default_pages})"
    );

    assert_page_matches_golden(&source, 0, "reading-page-large-relaxed-0");
}

/// The same blocks at the default size in the Heavier (SemiBold) weight:
/// regular and italic runs render one step heavier while bold emphasis keeps
/// the Bold face. Wider glyphs shift wrap points, so the frame differs from
/// the Normal-weight page; page 0 is pinned.
#[test]
fn reading_page_bodies_match_goldens_heavy() {
    let source = fixture(TypeSettings {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Heavy,
        family: FontFamily::Literata,
    });
    assert_page_matches_golden(&source, 0, "reading-page-heavy-0");
}

/// The default size and weight in Merriweather: every glyph comes from the
/// Merriweather faces and its advances shift wrap points, so the frame differs
/// from the Literata page; page 0 is pinned.
#[test]
fn reading_page_bodies_match_goldens_merriweather() {
    let source = fixture(TypeSettings {
        size: FontSize::Medium,
        spacing: LineSpacing::Normal,
        weight: FontWeight::Normal,
        family: FontFamily::Merriweather,
    });
    assert_page_matches_golden(&source, 0, "reading-page-merriweather-0");
}

/// The default grid fills the selected panel height: seventeen body lines on
/// X4 or nineteen on the 48-row-taller X3, with the next line spilling. Pins
/// both the 26px default advance and the ink-height fit rule that stops
/// charging a trailing paragraph gap against the page edge.
#[test]
fn default_grid_uses_selected_panel_height() {
    let paragraph_of = |lines: usize| -> FixtureBlocks {
        let blocks = (0..lines)
            .map(|index| FixtureBlock {
                record: record(TextRole::Body, TextAlign::Left, 1),
                text: "line".into(),
                style: FontStyle::Regular,
                page_break_before: false,
                paragraph_end: index == lines - 1,
            })
            .collect();
        FixtureBlocks {
            blocks,
            settings: TypeSettings::DEFAULT,
            portrait: false,
        }
    };
    let fitting_lines = if cfg!(feature = "device-x3") { 19 } else { 17 };
    assert_eq!(
        paginate_block_pages(&paragraph_of(fitting_lines)),
        1
    );
    assert_eq!(
        paginate_block_pages(&paragraph_of(fitting_lines + 1)),
        2
    );
}

/// Small/compact goes the other way: at least as much text per page.
#[test]
fn small_compact_paginates_no_worse_than_default() {
    let source = fixture(TypeSettings {
        size: FontSize::Small,
        spacing: LineSpacing::Compact,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
    });
    let default_pages =
        paginate_block_pages(&fixture(TypeSettings::DEFAULT));
    let pages = paginate_block_pages(&source);
    assert!(
        pages <= default_pages,
        "small/compact must not need more pages ({pages}) than default ({default_pages})"
    );
}

/// Pin the portrait reading surface: the fixture re-wrapped into the
/// upright page box, drawn through the portrait frame with the counter.
/// Catches wrap-width, page-walk, and footer placement regressions in the
/// orientation the panel was not built for.
#[test]
fn portrait_reading_surface_matches_golden() {
    let source = portrait_fixture();
    let pages = paginate_block_pages(&source);
    assert!(
        pages >= 2,
        "portrait fixture should span at least two pages, got {pages}"
    );

    let page = page_record_at(&source, 0);
    let mut fb = Framebuffer::new();
    fb.set_frame(display::fb::FbFrame::Portrait);
    fb.clear(true);
    draw_reading_page_body(&mut fb, &source, page);
    draw_reading_page_counter(&mut fb, "1/2");

    let actual = encode_png(&fb);
    let path = golden_path("reading-portrait-surface-0");
    if std::env::var("REGEN_READING_GOLDEN").is_ok() {
        std::fs::write(&path, &actual).expect("write golden");
        return;
    }
    let expected = std::fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden {} ({err}); run with REGEN_READING_GOLDEN=1 to create",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "portrait reading surface diverged from {}",
        path.display()
    );
}
