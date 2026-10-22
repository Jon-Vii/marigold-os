//! Golden-frame coverage for the shared reading surface: paginate and draw
//! synthetic cached blocks through `ui::reading` — the exact code firmware
//! uses for SD reading pages — and compare against checked-in frames.
//!
//! Golden images are pinned per-board (X4's 800x480 and X3's 792x528 each
//! get their own files, see `golden_path`); pixel comparisons run on both.
//!
//! Frames compare by decoded pixels; a mismatch writes `<name>.actual.png`
//! and `<name>.diff.png` under `target/golden-diff/` (see `src/golden.rs`).
//!
//! Regenerate after intentional typography changes with:
//! `REGEN_READING_GOLDEN=1 cargo test --manifest-path tools/emulator/Cargo.toml --target <host> --test reading_golden`
//! (repeat with `--features device-x3` to refresh the X3 frames too). Only
//! the value `1` regenerates.
#[path = "../src/golden.rs"]
mod golden;

use std::path::{Path, PathBuf};

use display::fb::Framebuffer;
use display::font::{
    style_marker_code, Alignment, FontFamily, FontSize, FontStyle, FontWeight, LineSpacing,
    TypeSettings, STYLE_MARKER,
};
use proto::cache::BlockRecord;
use proto::text::{TextAlign, TextRole};
use ui::reading::{
    block_first_line_indent, body_font, fit_image, paginate_block_pages, wrapped_line_count,
    PageBox, ReaderImage, ReadingBlocks,
};
use ui::reading::{
    draw_image_placeholder, draw_reading_page_body, draw_reading_page_counter,
    draw_reading_page_counter_aligned, page_record_at,
};

struct FixtureBlock {
    record: BlockRecord,
    text: String,
    style: FontStyle,
    page_break_before: bool,
    paragraph_end: bool,
    /// Whether the block opens a paragraph, when not derived from the
    /// previous block's end (a drop cap's line opens one unindented).
    paragraph_start: Option<bool>,
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

    fn paragraph_start(&self, index: usize) -> bool {
        match self
            .blocks
            .get(index)
            .and_then(|block| block.paragraph_start)
        {
            Some(start) => start,
            None => index == 0 || self.paragraph_end(index - 1),
        }
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
        image_height: 0,
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
        paragraph_start: None,
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
        paragraph_start: None,
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
        paragraph_start: None,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::BlockQuote, TextAlign::Left, 2),
        text: "A quoted aside, indented from the left margin and wrapped across more \
               than a single line to exercise the blockquote geometry."
            .into(),
        style: FontStyle::Italic,
        page_break_before: false,
        paragraph_end: true,
        paragraph_start: None,
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
            paragraph_start: None,
        });
    }
    blocks.push(FixtureBlock {
        record: record(TextRole::Heading2, TextAlign::Center, 1),
        text: styled(&[(FontStyle::Bold, "Forced Second Page")]),
        style: FontStyle::Bold,
        page_break_before: true,
        paragraph_end: true,
        paragraph_start: None,
    });
    blocks.push(FixtureBlock {
        record: record(TextRole::Body, TextAlign::Center, 1),
        text: styled(&[(FontStyle::Regular, "* * *")]),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end: true,
        paragraph_start: None,
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

fn golden_path(name: &str) -> PathBuf {
    let suffix = if cfg!(feature = "device-x3") {
        "-x3"
    } else {
        ""
    };
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(format!("{name}{suffix}.png"))
}

/// Compare `fb` with the named golden, or rewrite the golden when
/// `REGEN_READING_GOLDEN=1`.
fn assert_frame_matches_golden(fb: &Framebuffer, name: &str) {
    let path = golden_path(name);
    if golden::regen_requested("REGEN_READING_GOLDEN") {
        golden::write_golden(&path, fb).expect("write golden");
        return;
    }
    if let Err(err) = golden::check_frame(&path, fb) {
        panic!("{err}\n(after an intentional change, rerun with REGEN_READING_GOLDEN=1)");
    }
}

fn assert_page_matches_golden(source: &FixtureBlocks, page_index: usize, name: &str) {
    let page = page_record_at(source, page_index);
    assert!(page.block_count > 0, "page {page_index} should hold blocks");
    let mut fb = Framebuffer::new();
    draw_reading_page_body(&mut fb, source, page, draw_image_placeholder);
    assert_frame_matches_golden(&fb, name);
}

#[test]
fn reading_page_bodies_match_goldens() {
    let source = fixture(TypeSettings::DEFAULT);
    let pages = paginate_block_pages(&source);
    assert!(
        pages >= 2,
        "fixture should span at least two pages, got {pages}"
    );

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
    draw_reading_page_body(&mut fb, &source, page, draw_image_placeholder);
    draw_reading_page_counter(&mut fb, "1/2");

    assert_frame_matches_golden(&fb, "reading-surface-0");
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
        alignment: Alignment::Justified,
    });
    let default_pages = paginate_block_pages(&fixture(TypeSettings::DEFAULT));
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
        alignment: Alignment::Justified,
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
        alignment: Alignment::Justified,
    });
    assert_page_matches_golden(&source, 0, "reading-page-merriweather-0");
}

/// The default type with the Left alignment setting: lines the book set
/// justified draw at natural spacing with a ragged right edge, while
/// centred text keeps its own alignment; page 0 is pinned.
#[test]
fn reading_page_bodies_match_goldens_left() {
    let source = fixture(TypeSettings {
        alignment: Alignment::Left,
        ..TypeSettings::DEFAULT
    });
    assert_page_matches_golden(&source, 0, "reading-page-left-0");
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
                paragraph_start: None,
            })
            .collect();
        FixtureBlocks {
            blocks,
            settings: TypeSettings::DEFAULT,
            portrait: false,
        }
    };
    let fitting_lines = if cfg!(feature = "device-x3") { 19 } else { 17 };
    assert_eq!(paginate_block_pages(&paragraph_of(fitting_lines)), 1);
    assert_eq!(paginate_block_pages(&paragraph_of(fitting_lines + 1)), 2);
}

/// Small/compact goes the other way: at least as much text per page.
#[test]
fn small_compact_paginates_no_worse_than_default() {
    let source = fixture(TypeSettings {
        size: FontSize::Small,
        spacing: LineSpacing::Compact,
        weight: FontWeight::Normal,
        family: FontFamily::Literata,
        alignment: Alignment::Justified,
    });
    let default_pages = paginate_block_pages(&fixture(TypeSettings::DEFAULT));
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
    draw_reading_page_body(&mut fb, &source, page, draw_image_placeholder);
    draw_reading_page_counter(&mut fb, "1/2");

    assert_frame_matches_golden(&fb, "reading-portrait-surface-0");
}

/// Jan Steinke's orientation marker (640x900, 8-bit gray): arrows and labels
/// at its top and bottom, a thick bar on its left and a thin one on its
/// right, so a flip, mirror or misplacement shows in the golden.
const MARKER: &[u8] = include_bytes!("../../../fixtures/images/orientation-test.png");
/// The same marker at a third of the size (213x300; see
/// `fixtures/images/shrink_orientation_marker.py`): it fits every page box
/// unscaled, with lines of text above it on the same page.
const SMALL_MARKER: &[u8] = include_bytes!("../../../fixtures/images/orientation-small.png");
/// A 17 x 12 parallelogram, the size of the symbol Euclid sets inside a
/// sentence, and a 75 x 75 initial T, the size of Alice's drop caps (see
/// `fixtures/images/make_inline_images.py`).
const SYMBOL: &[u8] = include_bytes!("../../../fixtures/images/inline-symbol.png");
const INITIAL: &[u8] = include_bytes!("../../../fixtures/images/inline-initial.png");
/// Every image the fixtures show, by the slot their tokens name.
const MARKERS: [&[u8]; 4] = [SMALL_MARKER, MARKER, SYMBOL, INITIAL];

/// A text line whose descenders hang over where an image below would start.
fn descender_line(text: &str, paragraph_end: bool) -> FixtureBlock {
    FixtureBlock {
        record: record(TextRole::Body, TextAlign::Left, 1),
        text: text.into(),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end,
        paragraph_start: None,
    }
}

/// An image block laid out the way the firmware will: probe the PNG, fit it
/// with the layout's helper, and store the height in the record and the
/// slot, width and alt in the block's token.
fn image_block(slot: u16, page_box: PageBox, alt: &str) -> FixtureBlock {
    let info = proto::image::png::probe(MARKERS[usize::from(slot)]).expect("marker probes");
    let (width, height) = fit_image(page_box, info.width, info.height);
    let mut token = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
    let text = proto::image::token::encode_image_token(slot, width, Some(alt), &mut token)
        .expect("token fits");
    FixtureBlock {
        record: BlockRecord {
            image_height: height,
            ..record(TextRole::Body, TextAlign::Center, 1)
        },
        text: text.into(),
        style: FontStyle::Regular,
        page_break_before: false,
        paragraph_end: true,
        paragraph_start: None,
    }
}

/// Text lines dense with descenders directly above the small marker, a
/// line of the tallest marks (stacked Vietnamese, accented capitals) under
/// it, then the full-size marker, fitted to the page box, with a closing
/// line.
fn image_fixture(portrait: bool) -> FixtureBlocks {
    let page_box = PageBox::for_portrait(portrait);
    let blocks = vec![
        descender_line("Quietly, a pygmy yak jogged past", false),
        descender_line("gypsy figs, ripe quinces, jumpy", false),
        descender_line("gnu, pug, jay; gyp jig qypjgy gjpqy", true),
        image_block(0, page_box, "The marker, small"),
        descender_line(
            "Below: \u{1EB2}\u{1E4C}\u{C9}\u{C5} \u{1EB2}\u{1E4C}\u{C9}\u{C5} \u{1EB2}\u{1E4C}\u{C9}\u{C5} \u{1EB2}\u{1E4C}",
            true,
        ),
        image_block(1, page_box, "The marker, fitted to the page"),
        descender_line("After the page-sized marker.", true),
    ];
    FixtureBlocks {
        blocks,
        settings: TypeSettings::DEFAULT,
        portrait,
    }
}

/// Draws a marker into the rect layout reserved, through the decoder and
/// the draw the firmware uses: `PngDecoder` at exactly the rect's size
/// (never fitted again), Floyd-Steinberg dithered to 1 bpp rows as the
/// pack stores them, and those blitted into the frame a band at a time, as
/// the firmware streams them from the pack through its band buffer.
fn draw_marker(fb: &mut Framebuffer, image: &ReaderImage<'_>) {
    use display::fb::ImageRows;
    use proto::image::dither::Ditherer;
    use proto::image::png::{probe, PngDecoder};
    use proto::image::SinkError;

    let png = MARKERS[usize::from(image.slot.expect("token names a slot"))];
    let info = probe(png).expect("marker probes");
    let (width, height) = (u32::from(image.width), u32::from(image.height));
    // The decoders clear what they read: hand them dirty memory.
    let mut scratch = vec![0xA5u8; info.scratch_bytes(width)];
    let mut dither_scratch = vec![0xA5u8; Ditherer::scratch_bytes(width)];
    let mut ditherer = Ditherer::new(&mut dither_scratch, width).expect("ditherer");
    let stride = Ditherer::packed_len(width);
    let band_rows = display::BAND_BYTES / stride;
    let mut band = vec![0xA5u8; band_rows * stride];
    let mut inflate = Box::new(miniz_oxide::inflate::stream::InflateState::new(
        miniz_oxide::DataFormat::Zlib,
    ));
    let mut decoder = PngDecoder::new(&mut scratch, width, height).expect("decoder");
    let mut rows = 0;
    let mut sink = |y: u32, luma: &[u8]| -> Result<(), SinkError> {
        let y = y as usize;
        let at = y % band_rows;
        ditherer
            .row(luma, &mut band[at * stride..][..stride])
            .expect("dither row");
        rows += 1;
        if at + 1 == band_rows || rows == height {
            fb.blit(
                i32::from(image.x),
                i32::from(image.y),
                ImageRows {
                    width: width as usize,
                    stride,
                    first_row: y - at,
                    bits: &band[..(at + 1) * stride],
                },
            );
        }
        Ok(())
    };
    for chunk in png.chunks(1024) {
        decoder
            .feed(&mut inflate, chunk, &mut sink)
            .expect("marker decodes");
    }
    decoder
        .finish(&mut inflate, &mut sink)
        .expect("marker completes");
    assert_eq!(rows, height, "every row of the reserved rect");
}

/// Checks, with the page's text drawn and its images not yet, that no text
/// ink reached an image's rect or the rows just above and below it. With
/// `text_alongside`, the line above must also hang its descenders into the
/// image's top gap, and the line below reach its marks into the bottom gap:
/// close enough that without the gaps they would have met the image.
fn assert_text_clears_image(
    fb: &Framebuffer,
    (x, y, width, height): (i16, i16, u16, u16),
    text_alongside: bool,
    settings: TypeSettings,
) {
    let columns = x as usize..x as usize + usize::from(width);
    let inked = |row: i16| {
        columns
            .clone()
            .any(|column| !fb.pixel(column, row as usize))
    };
    let end = y + height as i16;
    for row in y - 1..=end {
        assert!(!inked(row), "text ink at row {row}, image rows {y}..{end}");
    }
    if text_alongside {
        let top_gap = ui::reading::image_top_gap(settings);
        let deepest = (y - 2 * top_gap..y).rev().find(|&row| inked(row));
        assert!(
            deepest.is_some_and(|row| row >= y - top_gap),
            "descenders should reach the top gap: deepest ink {deepest:?}, image at {y}"
        );
        let bottom_gap = ui::reading::image_bottom_gap(settings);
        let tallest = (end..end + 40).find(|&row| inked(row));
        assert!(
            tallest.is_some_and(|row| row <= end + bottom_gap),
            "marks should reach the bottom gap: tallest ink {tallest:?}, image ends {end}"
        );
    }
}

/// The lines of a justified paragraph laid out as the firmware's line
/// builder lays them out (greedily, each image one unit at its inline size,
/// never split), from `template`, whose `{2}` and `{3}` stand for inline
/// images in those slots. A paragraph whose first unit is a drop cap opens
/// unindented, the drop cap in the indent's place.
fn inline_paragraph(
    template: &str,
    page_box: PageBox,
    settings: TypeSettings,
) -> Vec<FixtureBlock> {
    use proto::image::token::{encode_inline_image_token, inline_image_tokens, InlineImageToken};
    let tokens: Vec<String> = [(2u16, 17u32, 12u32), (3, 75, 75)]
        .into_iter()
        .map(|(slot, width, height)| {
            let (width, height) = ui::reading::fit_inline_image(page_box, width, height);
            let mut out = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
            encode_inline_image_token(
                InlineImageToken {
                    slot,
                    width,
                    height,
                },
                &mut out,
            )
            .to_owned()
        })
        .collect();
    let text = template
        .replace("{2}", &tokens[0])
        .replace("{3}", &tokens[1]);
    let drop_cap = template.starts_with("{3}");
    let limit = page_box.right - ui::reading::READER_WRAP_SAFETY;
    let indent = ui::reading::paragraph_indent(settings.size);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let opens = lines.is_empty() && !drop_cap;
        let x = page_box.left + if opens { indent } else { 0 };
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty()
            || x + ui::reading::inline_line_ink_width(&candidate, settings, FontStyle::Regular)
                <= limit
        {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_owned()));
        }
    }
    lines.push(line);
    let count = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let tallest = inline_image_tokens(&text).map(|token| token.height).max();
            FixtureBlock {
                record: BlockRecord {
                    image_height: tallest.map_or(0, BlockRecord::inline_image_field),
                    ..record(TextRole::Body, TextAlign::Justify, 1)
                },
                text,
                style: FontStyle::Regular,
                page_break_before: false,
                paragraph_end: index + 1 == count,
                paragraph_start: (index == 0 && drop_cap).then_some(false),
            }
        })
        .collect()
}

/// Euclid's sentence with its symbol inside a line, a justified line with
/// symbols among its words, and Alice's chapter opening with its initial
/// standing in the first line, below a line of descenders. That first line
/// cannot take the dash-joined word after it beside the initial, and is
/// justified all the same.
fn inline_image_fixture(portrait: bool) -> FixtureBlocks {
    let page_box = PageBox::for_portrait(portrait);
    let settings = TypeSettings::DEFAULT;
    let mut blocks = inline_paragraph(
        "Dem.\u{2014}Complete the {2} AEBG. Draw DH, CI parallel to AG, BG. Join IH, and \
         produce; then AB, CD, IH are concurrent, and the {2} on the diagonal is equal to \
         the {2} beside it, which was to be shown.",
        page_box,
        settings,
    );
    blocks.push(descender_line("Quietly, a pygmy yak jogged past", true));
    blocks.extend(inline_paragraph(
        "{3}HEY were indeed a queer-looking party that assembled on the bank\u{2014}the \
         birds with draggled feathers, the animals with their fur clinging close to them, \
         and all dripping wet, cross, and uncomfortable.",
        page_box,
        settings,
    ));
    FixtureBlocks {
        blocks,
        settings,
        portrait,
    }
}

/// Inline images in every hold, on whichever panel this build is for: a
/// small symbol standing on the baseline mid-sentence, justified lines
/// whose gaps stretch around images, and a 64-row initial opening a
/// paragraph in the indent's place, its line grown to clear the line
/// above. Text is drawn first and checked clear of every image's rect and
/// the rows that keep it apart, then the images through the decoder and
/// the blit, as the firmware draws them.
#[test]
fn inline_image_pages_match_goldens() {
    use display::fb::FbFrame;
    for (frame, orientation) in [
        (FbFrame::Native, "landscape"),
        (FbFrame::Portrait, "portrait"),
        (FbFrame::LandscapeFlipped, "landscape-flipped"),
    ] {
        let portrait = frame == FbFrame::Portrait;
        let source = inline_image_fixture(portrait);
        assert_eq!(paginate_block_pages(&source), 1, "{orientation}");
        let page = page_record_at(&source, 0);
        let mut fb = Framebuffer::new();
        fb.set_frame(frame);
        let mut images = Vec::new();
        draw_reading_page_body(&mut fb, &source, page, |_, image| {
            images.push((image.x, image.y, image.width, image.height));
        });
        // Three symbols and the initial, each on its line's baseline.
        assert_eq!(images.len(), 4, "{orientation}");
        let mut baselines = Vec::new();
        ui::reading::for_each_drawable_block(&source, page, |block| {
            baselines.push((block.y, block.record.has_inline_images()));
            true
        });
        for &(x, y, width, height) in &images {
            assert!(
                baselines
                    .iter()
                    .any(|&(baseline, inline)| inline && y + height as i16 == baseline),
                "{orientation}: image at {x},{y} {width}x{height} stands on no baseline"
            );
            assert_text_clears_image(&fb, (x, y, width, height), false, source.settings);
        }
        let initial = images
            .iter()
            .find(|image| image.3 == 64)
            .expect("the initial");
        let page_box = PageBox::for_portrait(portrait);
        assert_eq!(
            initial.0, page_box.left,
            "the initial takes the indent's place"
        );
        ui::reading::for_each_drawable_block(&source, page, |block| {
            if let Some(image) = block.image {
                draw_marker(&mut fb, &image);
            }
            true
        });
        draw_reading_page_body(&mut fb, &source, page, |fb, image| draw_marker(fb, image));
        draw_reading_page_counter_aligned(&mut fb, "1/1", frame == FbFrame::LandscapeFlipped);
        assert_frame_matches_golden(&fb, &format!("reading-inline-{orientation}-0"));
    }
}

/// Image pages in every hold, on whichever panel this build is for: page 0
/// holds text with descenders directly over the small marker, shown at its
/// own size; page 1 the full marker, fitted to the page box. Both are
/// centred in the column and sit below the gap that keeps the line above
/// clear of them. Flipped landscape lays out as landscape does, and its
/// frame lands in the buffer mirrored left to right, so its goldens are the
/// landscape ones mirrored, apart from the counter, which that hold sets at
/// the left. Portrait goldens are stored turned a quarter.
#[test]
fn image_pages_match_goldens() {
    use display::fb::FbFrame;
    for (frame, orientation) in [
        (FbFrame::Native, "landscape"),
        (FbFrame::Portrait, "portrait"),
        (FbFrame::LandscapeFlipped, "landscape-flipped"),
    ] {
        let portrait = frame == FbFrame::Portrait;
        let source = image_fixture(portrait);
        let pages = paginate_block_pages(&source);
        assert!(pages >= 2, "{orientation}: {pages} pages");
        for page_index in 0..2 {
            let page = page_record_at(&source, page_index);
            let mut fb = Framebuffer::new();
            fb.set_frame(frame);
            // Text first, then the images: so the check below sees the
            // lines on both sides of each image before it is drawn.
            let mut images = Vec::new();
            draw_reading_page_body(&mut fb, &source, page, |_, image| {
                images.push((image.x, image.y, image.width, image.height));
            });
            for &image in &images {
                assert_text_clears_image(&fb, image, page_index == 0, source.settings);
            }
            ui::reading::for_each_drawable_block(&source, page, |block| {
                if let Some(image) = block.image {
                    draw_marker(&mut fb, &image);
                }
                true
            });
            draw_reading_page_counter_aligned(
                &mut fb,
                &format!("{}/{pages}", page_index + 1),
                frame == FbFrame::LandscapeFlipped,
            );
            assert_eq!(images.len(), 1, "{orientation} page {page_index}");
            let (x, _, width, height) = images[0];
            let page_box = PageBox::for_portrait(portrait);
            let column = page_box.right - page_box.left;
            assert_eq!(x, page_box.left + (column - width as i16) / 2, "centred");
            if page_index == 0 {
                assert_eq!((width, height), (213, 300), "shown at its own size");
            }
            assert_frame_matches_golden(&fb, &format!("reading-image-{orientation}-{page_index}"));
        }
    }
}
