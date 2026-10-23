//! The XHTML block sink (`fw::block_sink`): how a chapter's images become
//! image blocks or join the lines of their paragraphs, run through the
//! streaming parser against a book's image index on an in-memory card, as
//! a spine's layout runs on the device.
#![allow(dead_code, unused_imports, unexpected_cfgs)]
extern crate self as embassy_time;
extern crate self as esp_println;
extern crate self as hal_ext;
#[path = "../../hal-ext/src/nvm.rs"]
pub mod nvm;
pub use std::println;
#[path = "../../fw/src/block_sink.rs"]
mod block_sink;
#[path = "../../fw/src/book_images.rs"]
mod book_images;
#[path = "../../fw/src/custom_font.rs"]
mod custom_font;
#[path = "support/fat.rs"]
mod fat;
#[path = "support/jpeg.rs"]
mod jpeg;
#[path = "../../fw/src/reader_cache_files.rs"]
mod reader_cache_files;
#[path = "../../fw/src/reader_layout.rs"]
mod reader_layout;
#[path = "../../fw/src/reader_store.rs"]
mod reader_store;
#[path = "support/zip.rs"]
mod zip;
mod sd_session {
    use embedded_sdmmc::{Directory, Mode, TimeSource};
    pub fn remove_file_reclaiming_clusters<
        D,
        T,
        const ND: usize,
        const NF: usize,
        const NV: usize,
    >(
        d: &Directory<'_, D, T, ND, NF, NV>,
        name: &str,
    ) -> bool
    where
        D: embedded_sdmmc::BlockDevice,
        T: TimeSource,
    {
        let Ok(f) = d.open_file_in_dir(name, Mode::ReadWriteTruncate) else {
            return false;
        };
        if f.close().is_err() {
            return false;
        }
        d.delete_file_in_dir(name).is_ok()
    }
}

/// The sink times its section writes with `embassy_time`.
pub struct Instant(std::time::Instant);
impl Instant {
    pub fn now() -> Self {
        Self(std::time::Instant::now())
    }
    pub fn elapsed(&self) -> Duration {
        Duration(self.0.elapsed())
    }
}
pub struct Duration(std::time::Duration);
impl Duration {
    pub fn as_micros(&self) -> u64 {
        self.0.as_micros() as u64
    }
    pub fn as_millis(&self) -> u64 {
        self.0.as_millis() as u64
    }
}
/// What an idle build gives way to.
pub static BUTTON_PRESSES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

use block_sink::{LibraryBlockSink, LineInkCursor, PendingImages, Preparation, SpineRun};
use book_images::BookImages;
use display::font::{FontSize, FontStyle, LineSpacing, TypeSettings, STYLE_MARKER};
use embedded_sdmmc::{VolumeIdx, VolumeManager};
use proto::cache::BlockRecord;
use proto::epub::{StreamingXmlTokenizer, XhtmlBlockStreamParser, ZipInflateScratch, ZipStream};
use proto::image::index::read_image;
use proto::image::token::{
    decode_image_token, encode_image_token, inline_image_tokens, inline_pieces, InlineImageToken,
    InlinePiece, IMAGE_TOKEN_MARK,
};
use proto::text::{TextAlign, TextRole};
use reader_store::ReaderStore;
use std::cell::{Cell, RefCell};
use ui::reading::{
    fit_image, fit_inline_image, inline_line_ink_width, paragraph_indent, styled_text_ink_width,
    PageBox, READER_WRAP_SAFETY,
};
use zip::{zip, Archive};

const KEY: &str = "E0000001";
const ID: (u32, u32) = (1234, 5678);
const CHAPTER: &str = "OEBPS/text/ch01.xhtml";
/// 17 × 12, the size of Euclid's parallelogram.
const SYMBOL: &[u8] = include_bytes!("../../fixtures/images/inline-symbol.png");
/// 75 × 75, the size of Alice's drop caps.
const INITIAL: &[u8] = include_bytes!("../../fixtures/images/inline-initial.png");
/// 213 × 300.
const PLATE: &[u8] = include_bytes!("../../fixtures/images/orientation-small.png");

/// The images every test book holds, by name under `OEBPS/images/`.
/// `photo.jpg` is only a JPEG's first bytes: it fails its probe and shows
/// as its alt text. The other JPEGs lay out as PNGs do.
fn images() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("sym.png", SYMBOL.to_vec()),
        ("t.png", INITIAL.to_vec()),
        ("plate.png", PLATE.to_vec()),
        ("photo.jpg", vec![0xff, 0xd8, 0xff, 0xe0, 0, 16]),
        ("cover.jpg", jpeg::photo(300, 400, false)),
        ("tiny.jpg", jpeg::photo(17, 12, false)),
        ("initial.jpg", jpeg::photo(75, 75, false)),
    ]
}

/// Where `name` sits in [`images`].
fn image_at(name: &str) -> usize {
    images()
        .iter()
        .position(|(image, _)| *image == name)
        .unwrap()
}

/// What a chapter laid out to: the store's blocks, and the image index's
/// inline marks afterwards.
struct Laid {
    store: Box<ReaderStore>,
    /// Slot of each image in [`images`] order, and whether its entry
    /// carries the inline mark.
    slots: Vec<Option<(u16, bool)>>,
}

impl Laid {
    fn blocks(&self) -> Vec<(BlockRecord, String)> {
        (0..self.store.block_count)
            .map(|index| {
                (
                    self.store.blocks[index],
                    self.store.block_text(index).to_owned(),
                )
            })
            .collect()
    }

    fn slot(&self, name: &str) -> u16 {
        self.slots[image_at(name)].expect("in the index").0
    }

    fn marked_inline(&self, name: &str) -> bool {
        self.slots[image_at(name)].expect("in the index").1
    }
}

fn wrap(body: &str) -> String {
    format!("<html><head><title>t</title></head><body>{body}</body></html>")
}

/// Lays `body` out as a spine is on the device: a first pass that marks
/// the images no probe has read, their probes, and a second pass; a book
/// whose ZIP holds the [`images`] gets an image index, and with `images`
/// false the sink is given none, as for a book without them.
fn lay_out_with(body: &str, settings: TypeSettings, images: bool) -> Laid {
    let xhtml = wrap(body);
    let book_images = self::images();
    let mut files: Vec<(String, bool, &[u8])> = vec![(CHAPTER.into(), false, xhtml.as_bytes())];
    for (name, bytes) in &book_images {
        files.push((format!("OEBPS/images/{name}"), false, bytes));
    }
    let files: Vec<(&str, bool, &[u8])> = files
        .iter()
        .map(|(name, deflate, bytes)| (name.as_str(), *deflate, *bytes))
        .collect();
    let (bytes, _) = zip(&files);
    let disk = fat::Disk::new();
    let manager: VolumeManager<_, _, 8, 8, 1> =
        VolumeManager::new_with_limits(&disk, fat::Clock, 0);
    let volume = manager.open_volume(VolumeIdx(0)).unwrap();
    let root = volume.open_root_dir().unwrap();
    reader_cache_files::ensure_v2_cache_dirs(&root, KEY).unwrap();
    let reads = RefCell::new(Vec::new());
    let fails = Cell::new(None);
    let mut tail = [0u8; 4096];
    let mut zip = ZipStream::new(
        Archive {
            bytes: &bytes,
            reads: &reads,
            fails: &fails,
        },
        &mut tail,
    )
    .unwrap();
    let mut walk = vec![0xa5u8; 8192];
    let mut window = vec![0xa5u8; 24 * 1024];
    let mut compressed = vec![0xa5u8; 8192];
    let mut inflate = ZipInflateScratch::new();
    let dir = book_images::book_cache_dir(&root, KEY).expect("cache directory");
    let index =
        book_images::open_book_images(&dir, ID, &mut zip, &mut walk, &mut window, &mut false)
            .expect("index");
    let mut store = Box::new(ReaderStore::new());
    store.set_layout(settings, false);
    let mut metrics = custom_font::MetricCache::new();
    // Dirty, as the container scratch's tail is on the device.
    let mut pending = vec![0xa5u8; 3800];
    let mut path = [0xa5u8; proto::epub::MAX_ENTRY_NAME_BYTES];
    let (mut partial, mut micros) = (false, 0u64);
    for first_pass in [true, false] {
        store.clear_lines();
        let mut sink = LibraryBlockSink {
            library: &mut store,
            root: &root,
            sections_dir: None,
            source_identity: ID,
            section_base: 0,
            run: SpineRun::Prefix,
            run_full: false,
            font_metrics: &mut metrics,
            book_partial: &mut partial,
            write_micros: &mut micros,
            spine_index: 0,
            line: heapless::String::new(),
            line_ink: LineInkCursor::new(settings, FontStyle::Regular),
            line_role: TextRole::Body,
            line_align: TextAlign::Justify,
            line_style: FontStyle::Regular,
            pending_space: false,
            dropping_paragraph: false,
            hyphenation: proto::hyphen::Hyphenation::for_language("en"),
            hyphen_run: 0,
            line_hyphenated: false,
            stopped: false,
            preparation: Preparation::Request,
            interrupted: false,
            target_pages: 96,
            generate_toc_from_headings: false,
            generated_toc_for_spine: false,
            pages_dirty: true,
            write_failed: false,
            images: images.then_some(&index),
            chapter: CHAPTER,
            image_path: &mut path,
            first_pass,
            unprobed: None,
            images_met: 0,
            pending: PendingImages::new(&mut pending),
            paragraph_has_text: false,
            line_image_height: 0,
            line_unindented: false,
        };
        let mut tokenizer = StreamingXmlTokenizer::new();
        let mut parser = XhtmlBlockStreamParser::new(false);
        let mut parsed = Ok(());
        for chunk in xhtml.as_bytes().chunks(7) {
            parsed = tokenizer.feed_xhtml_blocks(chunk, &mut parser, None, &mut sink);
            if parsed.is_err() {
                break;
            }
        }
        if parsed.is_ok() {
            parsed = tokenizer.finish_xhtml_blocks(&mut parser, &mut sink);
        }
        assert!(!sink.write_failed && !sink.stopped, "{parsed:?}");
        if let (true, Some((first, last))) = (first_pass, sink.unprobed) {
            drop(sink);
            index
                .probe_marked(
                    &mut zip,
                    first..=last,
                    &mut compressed,
                    &mut window,
                    &mut inflate,
                    || false,
                )
                .expect("probed");
            continue;
        }
        parsed.expect("parsed");
        sink.finish_lines();
        break;
    }
    // The index's file is open once at a time.
    drop(index);
    let slots = book_images
        .iter()
        .map(|(name, _)| {
            let mut path = [0u8; proto::epub::MAX_ENTRY_NAME_BYTES];
            let name = proto::image::path::resolve_image_path(
                CHAPTER,
                &format!("../images/{name}"),
                &mut path,
            )
            .unwrap();
            let hash = proto::image::path::path_hash(name);
            let file = dir
                .open_file_in_dir(
                    proto::image::index::IMAGE_INDEX_FILE,
                    embedded_sdmmc::Mode::ReadOnly,
                )
                .ok()?;
            let header = proto::image::index::open_image_index(
                &mut book_images::CardIndex(&file),
                &book_images::image_index_key(ID),
            )
            .ok()?;
            let (slot, entry) =
                proto::image::index::find_image(&mut book_images::CardIndex(&file), &header, hash)
                    .ok()??;
            Some((slot, entry.inline))
        })
        .collect();
    Laid { store, slots }
}

fn lay_out(body: &str) -> Laid {
    lay_out_with(body, TypeSettings::DEFAULT, true)
}

fn token(slot: u16, (width, height): (u16, u16)) -> InlineImageToken {
    InlineImageToken {
        slot,
        width,
        height,
    }
}

/// A line's text with each image token shown as `[slot]`, style markers as
/// `{digit}`.
fn shown(text: &str) -> String {
    let mut out = String::new();
    for piece in inline_pieces(text) {
        match piece {
            InlinePiece::Text(run) => {
                let mut chars = run.chars();
                while let Some(ch) = chars.next() {
                    if ch == STYLE_MARKER {
                        out.push('{');
                        out.extend(chars.next());
                        out.push('}');
                    } else {
                        out.push(ch);
                    }
                }
            }
            InlinePiece::Image(token) => out.push_str(&format!("[{}]", token.slot)),
        }
    }
    out
}

const SYMBOL_SIZE: (u16, u16) = (17, 12);

#[test]
fn an_image_in_a_sentence_stays_in_its_line() {
    let laid = lay_out(r#"<p>Complete the <img alt="PICT" src="../images/sym.png"/> AEBG.</p>"#);
    let blocks = laid.blocks();
    let sym = laid.slot("sym.png");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    let (record, text) = &blocks[0];
    assert!(record.has_inline_images() && !record.is_image());
    assert_eq!(record.inline_image_height(), 12);
    assert_eq!(record.line_count, 1);
    assert_eq!(shown(text), format!("Complete the [{sym}] AEBG."));
    assert_eq!(
        inline_image_tokens(text).collect::<Vec<_>>(),
        [token(sym, SYMBOL_SIZE)]
    );
    // Small enough to show at one size either way: no inline mark.
    assert!(!laid.marked_inline("sym.png"));
}

#[test]
fn images_join_their_paragraph_at_its_start_middle_and_end() {
    let laid = lay_out(concat!(
        r#"<p><img alt="PICT" src="../images/sym.png"/> starts this one.</p>"#,
        r#"<p>In the <img alt="PICT" src="../images/sym.png"/>middle.</p>"#,
        r#"<p>And at the end <img alt="PICT" src="../images/sym.png"/></p>"#,
        r#"<p><i>Styled</i> <img alt="PICT" src="../images/sym.png"/> <b>runs</b>.</p>"#,
    ));
    let sym = laid.slot("sym.png");
    let lines: Vec<String> = laid.blocks().iter().map(|(_, text)| shown(text)).collect();
    assert_eq!(
        lines,
        [
            format!("[{sym}] starts this one."),
            format!("In the [{sym}]middle."),
            format!("And at the end [{sym}]"),
            // A space keeps the style before it; an image changes none.
            format!("{{1}}Styled [{sym}] {{2}}runs{{0}}."),
        ]
    );
    for (record, _) in laid.blocks() {
        assert!(record.has_inline_images());
        assert_eq!(record.inline_image_height(), 12);
    }
}

#[test]
fn an_image_alone_in_its_paragraph_stays_a_block() {
    let laid = lay_out(concat!(
        r#"<p>Before.</p>"#,
        r#"<p><img alt="A plate" src="../images/plate.png"/></p>"#,
        "<div>\n  <img alt=\"T\" src=\"../images/t.png\"/>\n</div>",
        r#"<p><img alt="" src="../images/sym.png"/> <img alt="x" src="../images/sym.png"/>&#160;</p>"#,
        r#"<p>After.</p>"#,
    ));
    let blocks = laid.blocks();
    let page_box = PageBox::LANDSCAPE;
    let expect_block = |index: usize, slot: u16, (width, height): (u16, u16), alt: &str| {
        let (record, text) = &blocks[index];
        assert!(record.is_image(), "{index}: {record:?}");
        assert_eq!(record.image_height, height, "{index}");
        assert_eq!(record.align, TextAlign::Center);
        let mut out = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
        assert_eq!(
            text.as_str(),
            encode_image_token(slot, width, Some(alt), &mut out).unwrap(),
            "{index}"
        );
    };
    assert_eq!(blocks.len(), 7, "{blocks:?}");
    assert_eq!(blocks[0].1, "Before.");
    expect_block(
        1,
        laid.slot("plate.png"),
        fit_image(page_box, 213, 300),
        "A plate",
    );
    // At its own size: alone in a paragraph, the 64-row cap does not apply.
    expect_block(2, laid.slot("t.png"), (75, 75), "T");
    // A paragraph of nothing but images (and spaces) holds two blocks.
    expect_block(3, laid.slot("sym.png"), SYMBOL_SIZE, "");
    expect_block(4, laid.slot("sym.png"), SYMBOL_SIZE, "x");
    // The no-break space after them makes the line it always made.
    assert_eq!(blocks[5].1, "\u{a0}");
    assert_eq!(blocks[6].1, "After.");
    assert!(!laid.marked_inline("t.png"));
}

#[test]
fn a_tall_image_inside_a_line_is_capped_and_marked_inline() {
    let laid = lay_out(r#"<p>A large <img alt="T" src="../images/t.png"/> initial.</p>"#);
    let t = laid.slot("t.png");
    let blocks = laid.blocks();
    assert_eq!(blocks.len(), 1);
    let (record, text) = &blocks[0];
    assert_eq!(fit_inline_image(PageBox::LANDSCAPE, 75, 75), (64, 64));
    assert_eq!(record.inline_image_height(), 64);
    assert_eq!(
        inline_image_tokens(text).collect::<Vec<_>>(),
        [token(t, (64, 64))]
    );
    // Its inline size is its own: it now decodes at that size.
    assert!(laid.marked_inline("t.png"));
}

/// A JPEG lays out as a PNG does: alone in its paragraph a block, inside
/// a sentence in its line, capped and marked inline when taller than a
/// line may hold. The `<image>` of an svg-wrapped cover becomes an image
/// block, which before JPEGs were decoded left nothing at all.
#[test]
fn jpegs_lay_out_as_pngs_do_and_an_svg_wrapped_cover_is_an_image() {
    let laid = lay_out(concat!(
        r#"<div class="cover"><svg xmlns="http://www.w3.org/2000/svg" "#,
        r#"xmlns:xlink="http://www.w3.org/1999/xlink" viewBox="0 0 300 400">"#,
        r#"<image width="300" height="400" xlink:href="../images/cover.jpg"/></svg></div>"#,
        r#"<p>Complete the <img alt="PICT" src="../images/tiny.jpg"/> AEBG.</p>"#,
        r#"<p>A large <img alt="T" src="../images/initial.jpg"/> initial.</p>"#,
        r#"<p><img alt="A plate" src="../images/cover.jpg"/></p>"#,
    ));
    let blocks = laid.blocks();
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    let cover = laid.slot("cover.jpg");
    let (width, height) = fit_image(PageBox::LANDSCAPE, 300, 400);
    let mut out = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
    let (record, text) = &blocks[0];
    assert!(record.is_image());
    assert_eq!(record.image_height, height);
    assert_eq!(
        text.as_str(),
        encode_image_token(cover, width, None, &mut out).unwrap()
    );
    let tiny = laid.slot("tiny.jpg");
    assert_eq!(shown(&blocks[1].1), format!("Complete the [{tiny}] AEBG."));
    assert_eq!(
        inline_image_tokens(&blocks[1].1).collect::<Vec<_>>(),
        [token(tiny, (17, 12))]
    );
    assert!(!laid.marked_inline("tiny.jpg"));
    let initial = laid.slot("initial.jpg");
    assert_eq!(
        inline_image_tokens(&blocks[2].1).collect::<Vec<_>>(),
        [token(initial, (64, 64))]
    );
    assert!(laid.marked_inline("initial.jpg"));
    let (record, text) = &blocks[3];
    assert!(record.is_image());
    assert_eq!(
        text.as_str(),
        encode_image_token(cover, width, Some("A plate"), &mut out).unwrap()
    );
}

#[test]
fn images_that_cannot_be_shown_leave_their_alt_in_the_line_in_italics() {
    let laid = lay_out(concat!(
        r#"<p>See <img alt="a photo" src="../images/photo.jpg"/> here.</p>"#,
        r#"<p>Then <img alt="" src="../images/photo.jpg"/> nothing.</p>"#,
        r#"<p>Or <img src="../images/missing.png"/> none.</p>"#,
        r#"<p><img alt="Title" src="../images/photo.jpg"/> leads.</p>"#,
        r#"<p><img alt="A photo, alone" src="../images/photo.jpg"/></p>"#,
    ));
    let lines: Vec<(TextAlign, String)> = laid
        .blocks()
        .iter()
        .map(|(record, text)| {
            assert!(!record.has_inline_images() && !record.is_image());
            (record.align, shown(text))
        })
        .collect();
    assert_eq!(
        lines,
        [
            // As between styled runs, a space keeps the style before it.
            (TextAlign::Justify, "See {1}a photo {0}here.".into()),
            (TextAlign::Justify, "Then nothing.".into()),
            (TextAlign::Justify, "Or {1}[Image] {0}none.".into()),
            // An alt that reads like a title-page label is still the alt.
            (TextAlign::Justify, "{1}Title {0}leads.".into()),
            // Alone, the placeholder it always was.
            (TextAlign::Center, "{1}A photo, alone".into()),
        ]
    );
}

#[test]
fn a_floated_drop_cap_starts_the_paragraph_after_it() {
    // Rackham's Alice, chapter III.
    let laid = lay_out(concat!(
        r#"<div class="chapter">CHAPTER III</div>"#,
        r#"<div class="figleft" style="width: 75px;">"#,
        "\n",
        r#"<img alt="T" src="../images/t.png" title=""/>"#,
        "\n",
        r#"</div><div class="unindent">HEY were indeed a queer-looking party.</div>"#,
        // A float whose alt is a caption is an illustration: it stays a
        // block.
        r#"<div class="figleft"><img alt="A plate" src="../images/plate.png"/></div>"#,
        r#"<p>Text beside it.</p>"#,
        // A drop cap a heading follows stays a block, as does one another
        // image follows.
        r#"<div class="figleft"><img alt="&quot;C" src="../images/t.png"/></div>"#,
        r#"<h2>A heading</h2>"#,
        r#"<div class="figleft"><img alt="A" src="../images/t.png"/></div>"#,
        r#"<p><img alt="" src="../images/plate.png"/></p>"#,
        // One that cannot be shown leaves its letter, in italics.
        r#"<div class="figleft"><img alt="I" src="../images/photo.jpg"/></div>"#,
        r#"<div class="unindent">T was the White Rabbit.</div>"#,
    ));
    let t = laid.slot("t.png");
    let plate = laid.slot("plate.png");
    let blocks = laid.blocks();
    let lines: Vec<String> = blocks
        .iter()
        .map(|(record, text)| match decode_image_token(text) {
            Some(token) if record.is_image() => format!("<{}>", token.slot),
            _ => shown(text),
        })
        .collect();
    assert_eq!(
        lines,
        [
            "CHAPTER III".to_owned(),
            format!("[{t}]HEY were indeed a queer-looking party."),
            format!("<{plate}>"),
            "Text beside it.".into(),
            format!("<{t}>"),
            "{2}A heading".into(),
            format!("<{t}>"),
            format!("<{plate}>"),
            "{1}I{0}T was the White Rabbit.".into(),
        ]
    );
    assert_eq!(blocks[1].0.inline_image_height(), 64);
    // The drop cap stands where the indent would: the line it opens takes
    // none, at layout or on the page.
    assert!(!laid.store.block_paragraph_start[1]);
    assert!(!laid.store.block_paragraph_start[8]);
    assert_eq!(ui::reading::block_first_line_indent(&*laid.store, 1), 0);
    assert_eq!(
        ui::reading::block_first_line_indent(&*laid.store, 3),
        paragraph_indent(FontSize::Medium)
    );
}

/// Every line of a laid-out paragraph fits the line builder's own limit,
/// and holds whole tokens.
fn assert_lines_fit(laid: &Laid, settings: TypeSettings) {
    let page_box = PageBox::LANDSCAPE;
    let limit = page_box.right - READER_WRAP_SAFETY;
    for (index, (record, text)) in laid.blocks().iter().enumerate() {
        let opens = laid.store.block_paragraph_start[index];
        let indent = if opens && matches!(record.align, TextAlign::Left | TextAlign::Justify) {
            paragraph_indent(settings.size)
        } else {
            0
        };
        let width = if record.has_inline_images() {
            inline_line_ink_width(text, settings, FontStyle::Regular)
        } else {
            styled_text_ink_width(text, settings, FontStyle::Regular)
        };
        assert!(
            page_box.left + indent + width <= limit,
            "line {index} overfull: {} > {limit}: {}",
            page_box.left + indent + width,
            shown(text)
        );
        // Every mark in the line starts a whole token.
        let marks = text.matches(IMAGE_TOKEN_MARK).count();
        assert_eq!(marks, inline_image_tokens(text).count(), "{}", shown(text));
        assert_eq!(record.has_inline_images(), marks > 0);
        if marks > 0 {
            let tallest = inline_image_tokens(text).map(|t| t.height).max().unwrap();
            assert_eq!(record.inline_image_height(), tallest);
        }
    }
}

#[test]
fn lines_with_images_wrap_them_whole_and_never_overfill() {
    // Long words around symbols, so lines end beside images and justified
    // prose hyphenates next to them.
    let sym = r#"<img alt="PICT" src="../images/sym.png"/>"#;
    let initial = r#"<img alt="T" src="../images/t.png"/>"#;
    let mut body = String::from("<p>");
    for round in 0..14 {
        body.push_str(&format!(
            "Notwithstanding the {sym} parallelogram{sym} constructions, \
             incomprehensibilities {sym}{sym} accumulate; therefore {initial} round {round} \
             demonstrates {sym}"
        ));
    }
    body.push_str("</p>");
    for settings in [
        TypeSettings::DEFAULT,
        TypeSettings {
            size: FontSize::Large,
            spacing: LineSpacing::Compact,
            ..TypeSettings::DEFAULT
        },
        TypeSettings {
            alignment: display::font::Alignment::Left,
            ..TypeSettings::DEFAULT
        },
    ] {
        let laid = lay_out_with(&body, settings, true);
        let blocks = laid.blocks();
        assert!(blocks.len() > 10, "{settings:?}: {} lines", blocks.len());
        assert_lines_fit(&laid, settings);
        // Every image of the source is placed, in order, and whole.
        let images: Vec<_> = blocks
            .iter()
            .flat_map(|(_, text)| inline_image_tokens(text).collect::<Vec<_>>())
            .collect();
        assert_eq!(images.len(), 14 * 6, "{settings:?}");
        let t = laid.slot("t.png");
        assert_eq!(images.iter().filter(|image| image.slot == t).count(), 14);
        // Justified prose still hyphenates beside images, and no line
        // ends in an image followed by a hyphen.
        let hyphenated = blocks
            .iter()
            .filter(|(_, text)| text.ends_with('-'))
            .count();
        if settings.alignment == display::font::Alignment::Justified {
            assert!(hyphenated > 0, "{settings:?}");
        }
        for (_, text) in &blocks {
            assert!(!shown(text).ends_with("]-"), "{}", shown(text));
        }
    }
}

#[test]
fn a_paragraph_of_only_images_is_blocks_however_many() {
    let sym = r#"<img alt="PICT" src="../images/sym.png"/> "#;
    let laid = lay_out(&format!("<p>{}</p><p>Then text.</p>", sym.repeat(40)));
    let blocks = laid.blocks();
    assert_eq!(blocks.len(), 41, "{} blocks", blocks.len());
    assert!(blocks[..40].iter().all(|(record, _)| record.is_image()));
    assert_eq!(blocks[40].1, "Then text.");
}

#[test]
fn many_images_before_the_first_text_still_keep_their_order() {
    // More than the sink holds back at once: those held go out as blocks,
    // the rest join the line, and none is lost or reordered.
    let sym = r#"<img alt="PICT" src="../images/sym.png"/>"#;
    let laid = lay_out(&format!("<p>{} text after.</p>", sym.repeat(40)));
    let blocks = laid.blocks();
    let images: usize = blocks
        .iter()
        .map(|(record, text)| {
            if record.is_image() {
                1
            } else {
                inline_image_tokens(text).count()
            }
        })
        .sum();
    assert_eq!(images, 40);
    let last = &blocks.last().unwrap().1;
    assert!(shown(last).ends_with(" text after."), "{}", shown(last));
    assert!(blocks.iter().any(|(record, _)| record.has_inline_images()));
}

#[test]
fn a_book_without_an_index_builds_as_it_always_did() {
    let body = concat!(
        r#"<p>Complete the <img alt="PICT" src="../images/sym.png"/> AEBG.</p>"#,
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<div>HEY were.</div>"#,
    );
    let laid = lay_out_with(body, TypeSettings::DEFAULT, false);
    let lines: Vec<(TextAlign, String)> = laid
        .blocks()
        .iter()
        .map(|(record, text)| {
            assert_eq!(record.image_height, 0);
            (record.align, shown(text))
        })
        .collect();
    // The paragraph ends at each image, which leaves its alt-text
    // placeholder, as before images had hooks.
    assert_eq!(
        lines,
        [
            (TextAlign::Justify, "Complete the".into()),
            (TextAlign::Center, "{1}PICT".into()),
            (TextAlign::Justify, "AEBG.".into()),
            (TextAlign::Center, "{1}T".into()),
            (TextAlign::Justify, "HEY were.".into()),
        ]
    );
}

#[test]
fn a_book_s_own_object_marks_never_pass_for_tokens() {
    let laid = lay_out(
        r#"<p>Text with a stray &#xFFFC;0001000c000c mark <img alt="PICT" src="../images/sym.png"/>.</p>"#,
    );
    let sym = laid.slot("sym.png");
    let blocks = laid.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        inline_image_tokens(&blocks[0].1).collect::<Vec<_>>(),
        [token(sym, SYMBOL_SIZE)]
    );
    assert_eq!(
        shown(&blocks[0].1),
        format!("Text with a stray 0001000c000c mark [{sym}].")
    );
}

/// Each block as a short string: an image block as `<slot>`, a line as its
/// text with `[slot]` for each image inside it and `{n}` for style markers.
fn outline(laid: &Laid) -> Vec<String> {
    laid.blocks()
        .iter()
        .map(|(record, text)| match decode_image_token(text) {
            Some(token) if record.is_image() => format!("<{}>", token.slot),
            _ => shown(text),
        })
        .collect()
}

#[test]
fn a_drop_cap_an_image_or_a_scene_break_follows_stays_a_block() {
    let laid = lay_out(concat!(
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<div><svg xmlns:xlink="http://www.w3.org/1999/xlink"><image xlink:href="../images/plate.png"/></svg></div>"#,
        r#"<p>HEY were.</p>"#,
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<p>&#160;</p><hr/><p>HEY again.</p>"#,
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<hr/><p>HEY once more.</p>"#,
    ));
    let (t, plate) = (laid.slot("t.png"), laid.slot("plate.png"));
    assert_eq!(
        outline(&laid),
        [
            format!("<{t}>"),
            format!("<{plate}>"),
            "HEY were.".into(),
            format!("<{t}>"),
            // The spacer line it always made stays.
            "\u{a0}".into(),
            "* * *".into(),
            "HEY again.".into(),
            format!("<{t}>"),
            "* * *".into(),
            "HEY once more.".into(),
        ]
    );
    assert!(!laid.marked_inline("t.png"));
}

#[test]
fn text_the_sanitizer_drops_is_no_text_for_an_image() {
    // Gutenberg's metadata and title-page labels are dropped, so an image
    // beside them is alone in its paragraph: a block at its own size, and
    // nothing of it glued to the next paragraph.
    let laid = lay_out(concat!(
        r#"<h2>Chapter</h2>"#,
        r#"<p>Produced by Someone <img alt="A plate" src="../images/plate.png"/></p>"#,
        r#"<p>Next.</p>"#,
        r#"<p><img alt="A plate" src="../images/plate.png"/>Produced by Someone</p>"#,
        r#"<p>Loose text after.</p>"#,
    ));
    let plate = laid.slot("plate.png");
    assert_eq!(
        outline(&laid),
        [
            "{2}Chapter".to_owned(),
            format!("<{plate}>"),
            "Next.".into(),
            format!("<{plate}>"),
            "Loose text after.".into(),
        ]
    );
    assert!(
        !laid.marked_inline("plate.png"),
        "never shown inside a line"
    );
    for (record, _) in laid.blocks() {
        assert!(!record.has_inline_images());
    }
}

#[test]
fn held_alt_text_is_kept_whole_however_many_images_wait() {
    // Over a hundred bytes, and short enough to sit on one line.
    let alt = format!("Gravure {}", "\u{e9}".repeat(50));
    assert!(alt.len() > 100);
    let image = format!(r#"<img alt="{alt}" src="../images/photo.jpg"/>"#);
    let laid = lay_out(&format!("<p>{}</p>", image.repeat(31)));
    let lines = outline(&laid);
    assert_eq!(lines.len(), 31, "{lines:?}");
    for line in lines {
        assert_eq!(line, format!("{{1}}{alt}"));
    }
    // And held until text follows, they all join the lines, whole (and
    // glued, as the images were).
    let laid = lay_out(&format!("<p>{} and text.</p>", image.repeat(3)));
    let text: String = outline(&laid).concat();
    assert_eq!(text.matches("Gravure").count(), 3, "{text}");
    assert_eq!(text.matches('\u{e9}').count(), 150, "{text}");
    assert!(text.ends_with(" {0}and text."), "{text}");
}

#[test]
fn spacing_beside_a_held_image_keeps_its_place() {
    let laid = lay_out(concat!(
        r#"<p>Before.</p>"#,
        r#"<p><img alt="x" src="../images/sym.png"/>&#160;</p>"#,
        r#"<p><img alt="x" src="../images/sym.png"/>&#8203;<i>text</i></p>"#,
        r#"<p><img alt="x" src="../images/sym.png"/><i>&#160;</i>glued</p>"#,
        r#"<p><img alt="x" src="../images/sym.png"/><i> </i>spaced</p>"#,
    ));
    let sym = laid.slot("sym.png");
    assert_eq!(
        outline(&laid),
        [
            "Before.".to_owned(),
            // Alone with a no-break space: the block and the spacer line,
            // as before.
            format!("<{sym}>"),
            "\u{a0}".into(),
            // A zero-width space adds no space.
            format!("[{sym}]{{1}}text"),
            // A no-break space stays one, between the image and its word.
            format!("[{sym}]{{1}}\u{a0}{{0}}glued"),
            format!("[{sym}] spaced"),
        ]
    );
}

#[test]
fn illustrations_stay_blocks_beside_captions_and_text() {
    let laid = lay_out(concat!(
        r#"<figure><img alt="A plate" src="../images/plate.png"/>"#,
        r#"<figcaption>The plate, a caption.</figcaption></figure>"#,
        r#"<div class="figcenter"><img alt="A plate" src="../images/plate.png"/> <span>Fig. 1</span></div>"#,
        r#"<p>Before the plate <img alt="A plate" src="../images/plate.png"/> and after it.</p>"#,
        r#"<p>A symbol <img alt="PICT" src="../images/sym.png"/> stays.</p>"#,
    ));
    let (plate, sym) = (laid.slot("plate.png"), laid.slot("sym.png"));
    assert_eq!(
        outline(&laid),
        [
            format!("<{plate}>"),
            "The plate, a caption.".into(),
            format!("<{plate}>"),
            "Fig. 1".into(),
            // The illustration ends the paragraph around it, as before.
            "Before the plate".into(),
            format!("<{plate}>"),
            "and after it.".into(),
            format!("A symbol [{sym}] stays."),
        ]
    );
    // At its own size every time, and never marked inline, so no block
    // use of it draws a shrunken decode.
    for (record, text) in laid.blocks() {
        if record.is_image() && decode_image_token(&text).unwrap().slot == plate {
            assert_eq!(
                record.image_height,
                fit_image(PageBox::LANDSCAPE, 213, 300).1
            );
        }
    }
    assert!(!laid.marked_inline("plate.png"));
    let blocks = laid.blocks();
    // The text after an illustration opens a paragraph, with its indent.
    assert!(laid.store.block_paragraph_end[4] && laid.store.block_paragraph_start[6]);
    assert!(blocks[6].0.align == TextAlign::Justify);
    assert!(ui::reading::fits_inline(75, 75) && ui::reading::fits_inline(17, 12));
    assert!(!ui::reading::fits_inline(213, 300) && !ui::reading::fits_inline(1000, 200));
}

#[test]
fn a_drop_cap_starts_only_a_word_of_body_text() {
    let laid = lay_out(concat!(
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<p>* * *</p><p>HEY after a typed break.</p>"#,
        r#"<div class="figleft"><img alt="T" src="../images/t.png"/></div>"#,
        r#"<ul><li>HEY four.</li></ul>"#,
    ));
    let t = laid.slot("t.png");
    assert_eq!(
        outline(&laid),
        [
            format!("<{t}>"),
            "* * *".into(),
            "HEY after a typed break.".into(),
            format!("<{t}>"),
            "- HEY four.".into(),
        ]
    );
    assert!(!laid.marked_inline("t.png"));
}

/// The rightmost ink of block `index` when its page is drawn.
fn line_ink_right(laid: &Laid, index: usize) -> i16 {
    let store = &*laid.store;
    let page = ui::reading::page_record_at(store, 0);
    assert!(index < usize::from(page.block_count));
    let mut fb = display::fb::Framebuffer::new();
    let mut baseline = 0;
    let mut at = 0;
    ui::reading::for_each_drawable_block(store, page, |block| {
        if at == index {
            baseline = block.y;
        }
        at += 1;
        true
    });
    ui::reading::draw_reading_page_body(&mut fb, store, page, ui::reading::draw_image_placeholder);
    (0..fb.frame_width() as i16)
        .rev()
        .find(|&x| (baseline - 20..baseline).any(|y| !fb.pixel(x as usize, y as usize)))
        .unwrap()
}

#[test]
fn a_drop_cap_s_first_line_fills_the_measure() {
    // Rackham's Alice, chapter III: the word after "on the" will not fit
    // beside the 64-row initial, leaving the line short by more than its
    // nine gaps could take at a text line's stretch.
    let body = concat!(
        r#"<div class="figleft" style="width: 75px;">"#,
        "\n",
        r#"<img alt="T" src="../images/t.png" title=""/>"#,
        "\n",
        r#"</div><div class="unindent">HEY were indeed a queer-looking party that assembled on "#,
        r#"the bank—the birds with draggled feathers, the animals with their fur clinging "#,
        r#"close to them, and all dripping wet, cross, and uncomfortable.</div>"#,
    );
    let right = PageBox::LANDSCAPE.right - READER_WRAP_SAFETY;
    let laid = lay_out(body);
    let first = shown(&laid.blocks()[0].1);
    assert!(first.ends_with("assembled on the"), "{first}");
    let natural = inline_line_ink_width(
        &laid.blocks()[0].1,
        TypeSettings::DEFAULT,
        FontStyle::Regular,
    );
    assert!(
        PageBox::LANDSCAPE.left + natural < right - 60,
        "the line is short by {natural}"
    );
    // Justified, its ink reaches the measure as its paragraph's next line
    // does.
    let (ink, next) = (line_ink_right(&laid, 0), line_ink_right(&laid, 1));
    assert!(
        (right - 4..right).contains(&ink) && (ink - next).abs() <= 2,
        "first line ends at {ink}, the next at {next}, measure {right}"
    );
    // Left aligned, it keeps its natural spacing.
    let left = TypeSettings {
        alignment: display::font::Alignment::Left,
        ..TypeSettings::DEFAULT
    };
    let laid = lay_out_with(body, left, true);
    assert!(line_ink_right(&laid, 0) < right - 40);
}
