//! Shared reading-surface layout: page bounds, type metrics, styled-text
//! measurement, and line wrapping. This is the "reader page plan" seam:
//! firmware reading views, cache building, and host preview tooling must
//! all agree on these numbers and this wrap behavior, so they live here.
//!
//! Measurement is incremental: width accumulates per character instead of
//! re-measuring a whole candidate line per word, which keeps wrapping O(n)
//! in text length.

use display::fb::{FbFrame, Framebuffer};
use display::font::{
    draw_text, family_weighted, fixed_ceil, fixed_round, literata, measure_text,
    style_from_marker_code, Alignment, BitmapFont, FontSize, FontStyle, LineSpacing, TypeSettings,
    STYLE_MARKER,
};
use proto::cache::{BlockRecord, PageRecord};
use proto::image::fit_inside;
use proto::image::token::{decode_image_token, inline_pieces, InlineImageToken, InlinePiece};
use proto::text::{TextAlign, TextRole};

/// The reading page's text box: wrap edges and the vertical page walk
/// bounds. Landscape keeps the historical `READER_*` numbers; portrait
/// stands the panel's long axis upright, so lines wrap at the short axis
/// and pages run the full long axis. Pagination, cache building, and
/// drawing all read the box from the [`ReadingBlocks`] source, so a store
/// can never wrap under one box and paginate under another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageBox {
    pub left: i16,
    pub right: i16,
    pub top: i16,
    pub bottom: i16,
}

impl PageBox {
    pub const LANDSCAPE: Self = Self {
        left: READER_LEFT_X,
        right: READER_RIGHT_X,
        top: READER_PAGE_TOP,
        bottom: READER_PAGE_BOTTOM,
    };

    /// Portrait keeps the same insets as landscape — 8 from the wrap
    /// edges, 6 above, 23 clear of the footer — inside the upright frame,
    /// with the footer lifted off the bezel (`PORTRAIT_FOOTER_LIFT`).
    pub const PORTRAIT: Self = Self {
        left: 8,
        right: FbFrame::Portrait.width() as i16 - 8,
        top: 6,
        bottom: FbFrame::Portrait.height() as i16 - 23 - PORTRAIT_FOOTER_LIFT,
    };

    pub const fn for_portrait(portrait: bool) -> Self {
        if portrait {
            Self::PORTRAIT
        } else {
            Self::LANDSCAPE
        }
    }

    /// Left text edge for a role: block quotes indent into the page.
    pub const fn x_for(self, role: TextRole) -> i16 {
        if matches!(role, TextRole::BlockQuote) {
            self.left + 24
        } else {
            self.left
        }
    }
}

/// Narrow read model for the reader page plan: bounded block records,
/// their cached text, and pagination flags. Firmware's ReaderStore and
/// host-side fixtures both implement it, so pagination and page drawing
/// cannot drift between device and tools.
pub trait ReadingBlocks {
    fn block_count(&self) -> usize;
    /// Record at `index` while it is inside the live block range.
    fn block(&self, index: usize) -> Option<BlockRecord>;
    fn block_text(&self, index: usize) -> &str;
    /// The block's stored style. It styles text without markers; text
    /// with markers draws as [`line_opening_style`] says.
    fn block_style(&self, index: usize) -> FontStyle;
    fn page_break_before(&self, index: usize) -> bool;
    fn paragraph_end(&self, index: usize) -> bool;
    /// True when block `index` opens a paragraph: the block right after a
    /// paragraph end, and the very first block. Only the opening line of a
    /// Body paragraph takes the first-line indent. The default derives it
    /// from `paragraph_end`; a store that carries a half-finished paragraph
    /// into the next section overrides this with a persisted flag, so a
    /// carried continuation line is not mistaken for a fresh paragraph.
    fn paragraph_start(&self, index: usize) -> bool {
        index == 0 || self.paragraph_end(index.wrapping_sub(1))
    }
    /// Type settings the blocks were laid out under. Every height,
    /// pagination, and drawing call in this module reads them from the
    /// source, so a store can never paginate with one size and draw with
    /// another.
    fn type_settings(&self) -> TypeSettings {
        TypeSettings::DEFAULT
    }
    /// The page box the blocks were laid out into. Reads like
    /// `type_settings`: heights, pagination, and drawing all follow the
    /// source's box, which changes with the orientation setting.
    fn page_box(&self) -> PageBox {
        PageBox::LANDSCAPE
    }
}

pub struct ReaderDrawableBlock<'a> {
    pub record: BlockRecord,
    pub text: &'a str,
    pub y: i16,
    pub advance: i16,
    /// The style the text opens in ([`line_opening_style`]), not the stored
    /// block style: every drawer and width measure starts from it.
    pub style: FontStyle,
    pub font: &'static BitmapFont,
    /// First-line indent for the block's opening line; 0 for continuation
    /// lines, headings, and centered text.
    pub indent: i16,
    /// The block ends its paragraph. A justified paragraph's last line
    /// keeps its natural spacing.
    pub paragraph_end: bool,
    /// Present for an image block, whose text is an image token rather
    /// than a line: draw the image here instead of the text.
    pub image: Option<ReaderImage<'a>>,
}

/// Where an image goes on the page: an image block's, or one inside a line
/// (with no alt, and its bottom row just above the line's baseline).
#[derive(Clone, Copy)]
pub struct ReaderImage<'a> {
    /// Top-left corner, in the drawing frame. A block's image is centred in
    /// the column and starts [`image_top_gap`] rows into its block.
    pub x: i16,
    pub y: i16,
    /// The fitted size layout reserved, which the decoder must emit
    /// exactly: never fit it again.
    pub width: u16,
    pub height: u16,
    /// The image's entry in the book's image index; `None` when the block's
    /// token cannot be read, which only damage to the cache can cause.
    pub slot: Option<u16>,
    /// Alt text, already cleaned; may be empty.
    pub alt: &'a str,
    /// The face the alt text draws in when the image cannot be shown: the
    /// body's italic, as the alt-text placeholder always was.
    pub alt_font: &'static BitmapFont,
}

/// Most rows [`image_top_gap`] keeps, whatever the type settings.
pub const IMAGE_TOP_GAP_MAX: i16 = 10;
/// Most rows [`image_bottom_gap`] keeps, whatever the type settings.
pub const IMAGE_BOTTOM_GAP_MAX: i16 = 5;
/// Rows of a page an image is fitted to leave free: both gaps at their
/// largest. Fitting against these rather than the current settings' gaps
/// makes the fitted size depend only on the page box, so it survives every
/// type change, and an image block always fits a page whatever the type.
pub const IMAGE_GAPS_MAX: i16 = IMAGE_TOP_GAP_MAX + IMAGE_BOTTOM_GAP_MAX;

/// White rows above an image: the body face's descent, plus one clear row,
/// so the line above's descenders never touch it. Text draws its baseline
/// at the bottom of its slot and its descenders spill into the next block,
/// which an image block's gap absorbs.
pub fn image_top_gap(settings: TypeSettings) -> i16 {
    let font = body_font(settings, FontStyle::Regular);
    (i16::from(font.line_height) - i16::from(font.baseline) + 1).clamp(1, IMAGE_TOP_GAP_MAX)
}

/// White rows below an image, so the next line's tallest glyph clears it by
/// a row. A line's ink can stand taller than its advance: capitals with
/// stacked marks (Vietnamese Ẳ, Ṍ) reach above the faces' own ascent, and
/// at the tighter spacings accented capitals (É, Å) reach the slot above.
/// Body lines have the smallest advance of any role, so they set the gap.
pub fn image_bottom_gap(settings: TypeSettings) -> i16 {
    IMAGE_BOTTOM_GAPS[settings.size as usize][settings.spacing as usize]
}

/// [`image_bottom_gap`] for every size (rows) and spacing (columns), worked
/// out at compile time. Worked out per block, the advances it needs became
/// lookup tables, which the ESP32-C3 build keeps in RAM.
const IMAGE_BOTTOM_GAPS: [[i16; 3]; 3] = {
    let sizes = [FontSize::Small, FontSize::Medium, FontSize::Large];
    let spacings = [
        LineSpacing::Compact,
        LineSpacing::Normal,
        LineSpacing::Relaxed,
    ];
    let mut gaps = [[0; 3]; 3];
    let mut size = 0;
    while size < sizes.len() {
        let reach = tallest_ink_above_baseline(sizes[size]) + 1;
        let mut spacing = 0;
        while spacing < spacings.len() {
            let gap = reach - body_line_advance(sizes[size], spacings[spacing]);
            gaps[size][spacing] = if gap < 0 {
                0
            } else if gap > IMAGE_BOTTOM_GAP_MAX {
                IMAGE_BOTTOM_GAP_MAX
            } else {
                gap
            };
            spacing += 1;
        }
        size += 1;
    }
    gaps
};

/// Rows the tallest glyph of any built-in body face reaches above its
/// baseline, at each size, across families, weights and styles. The
/// Vietnamese stacked marks set it. A test checks it against every glyph.
const fn tallest_ink_above_baseline(size: FontSize) -> i16 {
    match size {
        FontSize::Small => 24,
        FontSize::Medium => 28,
        FontSize::Large => 33,
    }
}

/// The size an image of `src_w × src_h` shows at in `page_box`: as large
/// as fits the column and the page less [`IMAGE_GAPS_MAX`], keeping its
/// shape, and never larger than the source. So an image block is never
/// taller than a page, and pagination cannot stall on one.
///
/// Layout calls this once per image and orientation, stores the result in
/// the block (height in the record, width in its token), and the decoder
/// emits exactly that size.
pub fn fit_image(page_box: PageBox, src_w: u32, src_h: u32) -> (u16, u16) {
    let column = (page_box.right - page_box.left).max(1) as u32;
    let rows = (page_box.bottom - page_box.top - IMAGE_GAPS_MAX).max(1) as u32;
    let (width, height) = fit_inside(src_w, src_h, column, rows);
    // Both fit the page box, so both fit u16.
    (width as u16, height as u16)
}

/// Tallest an image inside a line is shown, whatever the type settings, so
/// an image decodes at one size per orientation however the reader sets
/// type. A symbol in a sentence keeps its own size; a drop cap becomes a
/// large initial.
pub const INLINE_IMAGE_MAX_HEIGHT: u16 = 64;

/// Tallest an image may stand alone in a paragraph, in either orientation,
/// and still go inside a line when its paragraph has text: twice the
/// inline cap. Symbols and drop caps are well under it; a taller image is
/// an illustration (a plate with a caption beside it, say), which stays a
/// block and ends the paragraph around it rather than shrink to a line.
pub const INLINE_IMAGE_MAX_BLOCK_HEIGHT: u16 = 2 * INLINE_IMAGE_MAX_HEIGHT;

/// Whether an image of `src_w × src_h` may go inside a line: its block
/// size is no taller than [`INLINE_IMAGE_MAX_BLOCK_HEIGHT`] in either page
/// box, so every orientation decides the same and decodes it at one size.
pub fn fits_inline(src_w: u32, src_h: u32) -> bool {
    [PageBox::LANDSCAPE, PageBox::PORTRAIT]
        .into_iter()
        .all(|page_box| fit_image(page_box, src_w, src_h).1 <= INLINE_IMAGE_MAX_BLOCK_HEIGHT)
}

/// Columns of the page box an inline image may not use: the widest
/// first-line indent (the large size's), which takes more than a block
/// quote's 24, and the wrap margin. So an inline image always fits the
/// line it starts, whichever line that is.
const INLINE_IMAGE_INSET: i16 = 34 + READER_WRAP_SAFETY;
const _: () = assert!(INLINE_IMAGE_INSET - READER_WRAP_SAFETY >= 24);

/// The size an image of `src_w × src_h` shows at inside a line in
/// `page_box`: as large as fits [`INLINE_IMAGE_MAX_HEIGHT`] and the
/// narrowest line, keeping its shape, and never larger than the source.
/// Like [`fit_image`], it depends only on the page box.
pub fn fit_inline_image(page_box: PageBox, src_w: u32, src_h: u32) -> (u16, u16) {
    let column = (page_box.right - page_box.left - INLINE_IMAGE_INSET).max(1) as u32;
    let (width, height) = fit_inside(src_w, src_h, column, u32::from(INLINE_IMAGE_MAX_HEIGHT));
    (width as u16, height as u16)
}

/// Rows a line's tallest inline image takes, as its record says, and never
/// more than [`INLINE_IMAGE_MAX_HEIGHT`] even when a damaged record says
/// otherwise.
fn inline_rows(record: BlockRecord) -> i16 {
    record.inline_image_height().min(INLINE_IMAGE_MAX_HEIGHT) as i16
}

/// Rows from the top of a line holding inline images down to its baseline:
/// the line advance, or the tallest image with the block images' top gap
/// above it when that is more, so the line above's descenders clear it by a
/// row. Below the baseline the line keeps [`image_bottom_gap`] for the line
/// below's tallest marks.
pub fn inline_line_ascent(settings: TypeSettings, record: BlockRecord) -> i16 {
    line_advance(settings, record.role).max(image_top_gap(settings) + inline_rows(record))
}

/// Rows an image block's image takes: its recorded height, which layout
/// fitted to the page box, and never more than [`fit_image`] can give even
/// when a damaged record says otherwise. So with its gaps the block fits a
/// page, and its rect never passes the page box.
fn image_rows(page_box: PageBox, record: BlockRecord) -> i16 {
    let most = (page_box.bottom - page_box.top - IMAGE_GAPS_MAX).max(1);
    i16::try_from(record.image_height).map_or(most, |rows| rows.min(most))
}

pub fn block_height(source: &impl ReadingBlocks, index: usize) -> i16 {
    let Some(record) = source.block(index) else {
        return 0;
    };
    let settings = source.type_settings();
    if record.is_image() {
        return image_top_gap(settings)
            + image_rows(source.page_box(), record)
            + image_bottom_gap(settings)
            + paragraph_gap_after(source, index);
    }
    if record.has_inline_images() {
        return inline_line_ascent(settings, record)
            + image_bottom_gap(settings)
            + paragraph_gap_after(source, index);
    }
    let advance = line_advance(settings, record.role);
    let height = if record.line_count == 1 {
        advance
    } else {
        wrapped_block_height(
            body_font(settings, source.block_style(index)),
            source.block_text(index),
            record.role,
            record.align,
            advance,
            block_first_line_indent(source, index),
            source.page_box(),
        )
    };
    height + paragraph_gap_after(source, index)
}

/// Block height without the trailing paragraph gap: the rows the block's
/// own ink occupies. Pagination charges this against the page edge — the
/// gap only separates blocks that share a page — while the cursor still
/// advances by the gapped height.
pub fn block_ink_height(source: &impl ReadingBlocks, index: usize) -> i16 {
    block_height(source, index) - paragraph_gap_after(source, index)
}

pub fn paragraph_gap_after(source: &impl ReadingBlocks, index: usize) -> i16 {
    if source.paragraph_end(index) {
        paragraph_gap(
            source
                .block(index)
                .map(|record| record.role)
                .unwrap_or(TextRole::Body),
        )
    } else {
        0
    }
}

/// Allocation-free cursor over complete page records. It does not borrow the
/// source between calls, so a bounded ReaderStore can write each result into
/// its page index without copying its blocks or allocating another page array.
/// Keep the source's blocks and layout unchanged until the walk finishes.
#[derive(Default)]
pub struct PageCursor {
    next_block: usize,
}

impl PageCursor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resume at a known page boundary. Earlier blocks are not visited;
    /// callers must retain the layout that established this boundary.
    pub fn from_page_start(first_block: usize) -> Self {
        Self {
            next_block: first_block,
        }
    }

    pub fn next_page(&mut self, source: &impl ReadingBlocks) -> Option<PageRecord> {
        let first_block = self.next_block;
        if first_block >= source.block_count() {
            return None;
        }
        let bounds = source.page_box();
        let mut y = i32::from(bounds.top);
        while self.next_block < source.block_count() {
            let index = self.next_block;
            // Measure once: block_ink_height would wrap the same text again.
            let height = i32::from(block_height(source, index));
            let ink_height = height - i32::from(paragraph_gap_after(source, index));
            if (y + ink_height > i32::from(bounds.bottom) || source.page_break_before(index))
                && y > i32::from(bounds.top)
            {
                break;
            }
            self.next_block += 1;
            y += height;
        }
        Some(PageRecord {
            first_block: first_block as u16,
            block_count: (self.next_block - first_block) as u16,
        })
    }
}

/// Count pages with the same walk used to build the firmware's bounded index.
/// An empty source still advertises one blank reading page.
pub fn paginate_block_pages(source: &impl ReadingBlocks) -> usize {
    let mut cursor = PageCursor::new();
    let mut pages = 0;
    while cursor.next_page(source).is_some() {
        pages += 1;
    }
    pages.max(1)
}

/// Resolve a page with the index-building walk, clamping to the last page.
pub fn page_record_at(source: &impl ReadingBlocks, page_index: usize) -> PageRecord {
    let mut cursor = PageCursor::new();
    let mut last = PageRecord {
        first_block: 0,
        block_count: 0,
    };
    for _ in 0..=page_index {
        let Some(page) = cursor.next_page(source) else {
            break;
        };
        last = page;
    }
    last
}

/// The loading bookplate shared by the firmware and host reading views.
/// Coordinates follow the framebuffer's current drawing frame; long labels
/// keep their leading text and an ellipsis inside the page margins.
pub fn draw_reading_bookplate(fb: &mut Framebuffer, title: &str, author: &str) {
    let max_width = fb.frame_width().saturating_sub(16) as u16;
    let title_y = fb.frame_height() as i16 / 2 - 8;
    for (style, text, y) in [
        (FontStyle::Bold, title, title_y),
        (FontStyle::Italic, author, title_y + 36),
    ] {
        if text.is_empty() {
            continue;
        }
        let font = literata(style);
        let truncated = measure_text(font, text) > max_width;
        let dots_width = if truncated {
            measure_text(font, "...")
        } else {
            0
        };
        let shown = crate::render::fit_text(font, text, max_width.saturating_sub(dots_width));
        let shown_width = measure_text(font, shown);
        let x = (fb.frame_width() as i16 - shown_width as i16 - dots_width as i16) / 2;
        draw_text(fb, font, shown, x, y, false);
        if truncated {
            draw_text(fb, font, "...", x + shown_width as i16, y, false);
        }
    }
}

pub fn for_each_drawable_block(
    source: &impl ReadingBlocks,
    page: PageRecord,
    mut visit: impl FnMut(ReaderDrawableBlock<'_>) -> bool,
) {
    let settings = source.type_settings();
    let page_box = source.page_box();
    let mut y = page_box.top;
    for offset in 0..page.block_count as usize {
        let index = page.first_block as usize + offset;
        let Some(record) = source.block(index) else {
            break;
        };
        let text = source.block_text(index);
        let advance = line_advance(settings, record.role);
        let style = line_opening_style(text, source.block_style(index));
        let height = block_height(source, index);
        if y + block_ink_height(source, index) > page_box.bottom && y > page_box.top {
            break;
        }
        let image = record
            .is_image()
            .then(|| place_image(settings, page_box, y, record, text));
        let baseline = if record.has_inline_images() {
            y + inline_line_ascent(settings, record)
        } else {
            y + advance
        };
        if !visit(ReaderDrawableBlock {
            record,
            text,
            y: baseline,
            advance,
            style,
            font: body_font(settings, style),
            indent: block_first_line_indent(source, index),
            paragraph_end: source.paragraph_end(index),
            image,
        }) {
            break;
        }
        y += height;
    }
}

/// Whether `page` holds an image block or a line with images inside it,
/// the only blocks that need more than RAM to draw: their pixels are on
/// the card.
pub fn page_has_images(source: &impl ReadingBlocks, page: PageRecord) -> bool {
    (0..page.block_count as usize).any(|offset| {
        source
            .block(page.first_block as usize + offset)
            .is_some_and(|record| record.is_image() || record.has_inline_images())
    })
}

/// Calls `visit` with every image `page` shows, block images and the images
/// inside its lines, in reading order: its slot and the size it draws at.
/// Stops when `visit` returns false. Measures no text.
pub fn for_each_page_image(
    source: &impl ReadingBlocks,
    page: PageRecord,
    mut visit: impl FnMut(u16, u16, u16) -> bool,
) {
    let column = (source.page_box().right - source.page_box().left).max(0) as u16;
    for_each_drawable_block(source, page, |block| {
        if let Some(image) = block.image {
            return image
                .slot
                .is_none_or(|slot| visit(slot, image.width, image.height));
        }
        if !block.record.has_inline_images() {
            return true;
        }
        let rows = inline_rows(block.record) as u16;
        inline_pieces(block.text).all(|piece| match piece {
            InlinePiece::Image(token) => {
                let (width, height) = inline_image_size(token, column, rows);
                visit(token.slot, width, height)
            }
            InlinePiece::Text(_) => true,
        })
    });
}

/// The size an inline image draws at: its token's, kept inside the column
/// and the line's recorded height, which only a damaged section can
/// exceed.
fn inline_image_size(token: InlineImageToken, column: u16, rows: u16) -> (u16, u16) {
    (token.width.min(column), token.height.min(rows))
}

/// An image block's image, for a block starting at row `top`: centred in
/// the column by the width its token records, below the top gap.
fn place_image<'a>(
    settings: TypeSettings,
    page_box: PageBox,
    top: i16,
    record: BlockRecord,
    text: &'a str,
) -> ReaderImage<'a> {
    let column = (page_box.right - page_box.left).max(0) as u16;
    let token = decode_image_token(text);
    let width = token.map_or(column, |token| token.width.min(column));
    ReaderImage {
        x: page_box.left + ((column - width) / 2) as i16,
        y: top + image_top_gap(settings),
        width,
        height: image_rows(page_box, record) as u16,
        slot: token.map(|token| token.slot),
        alt: token.map_or("", |token| token.alt),
        alt_font: body_font(settings, FontStyle::Italic),
    }
}

/// Draws an image that cannot be shown: its outline, with the alt text on
/// one line inside, cut to fit, when the box is tall enough for a line.
/// The image drawer to pass to [`draw_reading_page_body`] when there are
/// no image pixels to draw.
pub fn draw_image_placeholder(fb: &mut Framebuffer, image: &ReaderImage<'_>) {
    const INSET: i16 = 4;
    let (Ok(x), Ok(y)) = (u16::try_from(image.x), u16::try_from(image.y)) else {
        return;
    };
    display::render::stroke_rect(
        fb,
        display::Rect::new(x, y, image.width, image.height),
        false,
    );
    let font = image.alt_font;
    let room = i32::from(image.width) - 2 * i32::from(INSET);
    if image.alt.is_empty()
        || room <= 0
        || i32::from(image.height) < i32::from(font.line_height) + 2 * i32::from(INSET)
    {
        return;
    }
    let shown = crate::render::fit_text(font, image.alt, room as u16);
    let shown_width = measure_text(font, shown) as i16;
    let x = image.x + (image.width as i16 - shown_width) / 2;
    let baseline = image.y
        + (image.height as i16 - i16::from(font.line_height)) / 2
        + i16::from(font.baseline);
    draw_text(fb, font, shown, x, baseline, false);
}

/// Draw one page of reading-body blocks: the single rendering of cached
/// reader content shared by firmware views and host tooling.
///
/// Image blocks draw nothing themselves: `draw_image` is called with each
/// one's placement and draws whatever it has, the image's rows or
/// [`draw_image_placeholder`].
pub fn draw_reading_page_body(
    fb: &mut Framebuffer,
    source: &impl ReadingBlocks,
    page: PageRecord,
    mut draw_image: impl FnMut(&mut Framebuffer, &ReaderImage<'_>),
) {
    let settings = source.type_settings();
    let page_box = source.page_box();
    for_each_drawable_block(source, page, |block| {
        if let Some(image) = block.image {
            draw_image(fb, &image);
            return true;
        }
        if block.record.has_inline_images() {
            draw_inline_line(fb, source, &block, &mut draw_image);
            return true;
        }
        let role = block.record.role;
        match effective_align(block.record.align, settings) {
            TextAlign::Left => {
                let x = page_box.x_for(role);
                if block.record.line_count == 1 {
                    draw_styled_line(
                        fb,
                        settings,
                        block.text,
                        x + block.indent,
                        block.y,
                        block.style,
                    );
                } else {
                    draw_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        page_box.right,
                        block.advance,
                        block.indent,
                    );
                }
            }
            TextAlign::Justify => {
                let x = page_box.x_for(role);
                if block.record.line_count == 1 {
                    if block.paragraph_end {
                        draw_styled_line(
                            fb,
                            settings,
                            block.text,
                            x + block.indent,
                            block.y,
                            block.style,
                        );
                    } else {
                        draw_justified_styled_line(
                            fb,
                            settings,
                            block.text,
                            x + block.indent,
                            block.y,
                            page_box.right,
                            block.style,
                        );
                    }
                } else {
                    draw_justified_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        x,
                        block.y,
                        page_box.right,
                        block.advance,
                        block.indent,
                    );
                }
            }
            TextAlign::Center => {
                if block.record.line_count == 1 {
                    let width = styled_text_ink_width(block.text, settings, block.style)
                        .min(page_box.right - page_box.left);
                    let x = ((page_box.left + page_box.right - width) / 2).max(page_box.left);
                    draw_styled_line(fb, settings, block.text, x, block.y, block.style);
                } else {
                    draw_centered_wrapped_literata(
                        fb,
                        block.font,
                        block.text,
                        block.y,
                        page_box.right - page_box.left,
                        block.advance,
                    );
                }
            }
        };
        true
    });
}

/// What placing a line's inline images needs: the column, which no image
/// is drawn wider than, the line's recorded image height, which none is
/// drawn taller than (only a damaged section can say otherwise), and the
/// italic face for an image drawn as a placeholder.
#[derive(Clone, Copy)]
pub struct InlineImages {
    pub column: u16,
    pub rows: u16,
    pub alt_font: &'static BitmapFont,
}

impl InlineImages {
    /// For a line whose record [`BlockRecord::has_inline_images`].
    pub fn for_line(source: &impl ReadingBlocks, record: BlockRecord) -> Self {
        let page_box = source.page_box();
        Self {
            column: (page_box.right - page_box.left).max(0) as u16,
            rows: inline_rows(record) as u16,
            alt_font: body_font(source.type_settings(), FontStyle::Italic),
        }
    }

    /// The image `token` names, drawn from column `x` with its bottom row
    /// just above `baseline`, so it stands on the line as its text does.
    pub fn place(&self, token: InlineImageToken, x: i16, baseline: i16) -> ReaderImage<'static> {
        let (width, height) = inline_image_size(token, self.column, self.rows);
        ReaderImage {
            x,
            y: baseline.saturating_sub(height as i16),
            width,
            height,
            slot: Some(token.slot),
            alt: "",
            alt_font: self.alt_font,
        }
    }
}

/// The natural width of a line holding inline images: its text measured as
/// [`styled_text_ink_width`] does, each image by its width. The line
/// builder measures a line the same way as it fills it.
pub fn inline_line_ink_width(text: &str, settings: TypeSettings, default_style: FontStyle) -> i16 {
    let mut cursor = StyledInkCursor::new(settings, default_style);
    for piece in inline_pieces(text) {
        match piece {
            InlinePiece::Text(run) => cursor.push_str(run),
            InlinePiece::Image(token) => cursor.push_object(token.width),
        }
    }
    cursor.width()
}

/// Draws a line that holds inline images at natural spacing from `x`, as
/// [`draw_styled_line`] draws one without: its runs of text on the
/// baseline, the style carrying on past each image, and each image handed
/// to `draw_image` where it stands. Returns the pen's column after it.
#[allow(clippy::too_many_arguments)]
pub fn draw_inline_styled_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    images: &InlineImages,
    text: &str,
    x: i16,
    baseline_y: i16,
    default_style: FontStyle,
    draw_image: &mut dyn FnMut(&mut Framebuffer, &ReaderImage<'_>),
) -> i16 {
    let mut cursor_x = x;
    let mut style = default_style;
    for piece in inline_pieces(text) {
        let run = match piece {
            InlinePiece::Image(token) => {
                let image = images.place(token, cursor_x, baseline_y);
                draw_image(fb, &image);
                cursor_x = cursor_x.saturating_add(image.width as i16);
                continue;
            }
            InlinePiece::Text(run) => run,
        };
        let mut run_start = 0usize;
        let mut iter = run.char_indices();
        while let Some((index, ch)) = iter.next() {
            if ch != STYLE_MARKER {
                continue;
            }
            if run_start < index {
                cursor_x = draw_text(
                    fb,
                    body_font(settings, style),
                    &run[run_start..index],
                    cursor_x,
                    baseline_y,
                    false,
                );
            }
            if let Some((code_index, code)) = iter.next() {
                style = style_from_marker_code(code).unwrap_or(style);
                run_start = code_index + code.len_utf8();
            } else {
                run_start = index + ch.len_utf8();
            }
        }
        if run_start < run.len() {
            cursor_x = draw_text(
                fb,
                body_font(settings, style),
                &run[run_start..],
                cursor_x,
                baseline_y,
                false,
            );
        }
    }
    cursor_x
}

/// [`draw_justified_styled_line`] for a line holding inline images: the
/// leftover width spreads over its inter-word spaces, an image counting as
/// a word, unless [`justify_extra`] declines.
#[allow(clippy::too_many_arguments)]
pub fn draw_justified_inline_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    images: &InlineImages,
    text: &str,
    x: i16,
    baseline_y: i16,
    right: i16,
    default_style: FontStyle,
    draw_image: &mut dyn FnMut(&mut Framebuffer, &ReaderImage<'_>),
) -> i16 {
    let gaps = justify_gap_count(text);
    let natural = inline_line_ink_width(text, settings, default_style);
    let Some(extra) = justify_inline_extra(x, natural, right, gaps) else {
        return draw_inline_styled_line(
            fb,
            settings,
            images,
            text,
            x,
            baseline_y,
            default_style,
            draw_image,
        );
    };
    let per_gap = extra / gaps as i16;
    let mut remainder = extra % gaps as i16;
    let mut cursor_x = x;
    walk_styled_words(text, default_style, |step| {
        match step {
            StyledWordStep::Word(word, style) => {
                cursor_x = draw_inline_styled_line(
                    fb, settings, images, word, cursor_x, baseline_y, style, draw_image,
                );
            }
            StyledWordStep::Gap(style) => {
                cursor_x += measure_text(body_font(settings, style), " ") as i16 + per_gap;
                if remainder > 0 {
                    cursor_x += 1;
                    remainder -= 1;
                }
            }
        }
        true
    });
    cursor_x
}

/// Draws a line that holds inline images, aligned as its record says. Out
/// of line, so a page without them draws in the frame it always had.
#[inline(never)]
fn draw_inline_line(
    fb: &mut Framebuffer,
    source: &impl ReadingBlocks,
    block: &ReaderDrawableBlock<'_>,
    draw_image: &mut dyn FnMut(&mut Framebuffer, &ReaderImage<'_>),
) {
    let settings = source.type_settings();
    let page_box = source.page_box();
    let images = InlineImages::for_line(source, block.record);
    let x = page_box.x_for(block.record.role) + block.indent;
    match effective_align(block.record.align, settings) {
        TextAlign::Justify if !block.paragraph_end => {
            draw_justified_inline_line(
                fb,
                settings,
                &images,
                block.text,
                x,
                block.y,
                page_box.right,
                block.style,
                draw_image,
            );
        }
        TextAlign::Left | TextAlign::Justify => {
            draw_inline_styled_line(
                fb,
                settings,
                &images,
                block.text,
                x,
                block.y,
                block.style,
                draw_image,
            );
        }
        TextAlign::Center => {
            let width = inline_line_ink_width(block.text, settings, block.style)
                .min(page_box.right - page_box.left);
            let x = ((page_box.left + page_box.right - width) / 2).max(page_box.left);
            draw_inline_styled_line(
                fb,
                settings,
                &images,
                block.text,
                x,
                block.y,
                block.style,
                draw_image,
            );
        }
    }
}

/// Draw the page-in-chapter counter that completes the reading surface.
/// Callers own the chapter-position calculation and formatting; this shared
/// seam owns the exact font, right inset, and panel-relative baseline.
pub fn draw_reading_page_counter(fb: &mut Framebuffer, label: &str) {
    draw_reading_page_counter_aligned(fb, label, false);
}

/// Rows the portrait footer (and page bottom) stand above the frame's
/// bottom edge beyond landscape's. That edge is the panel's short side,
/// where landscape keeps an 8-pixel text margin: its first columns sit
/// under the bezel, and a counter 3 rows off it lost its lowest ink.
pub const PORTRAIT_FOOTER_LIFT: i16 = 8;

pub fn draw_reading_page_counter_aligned(fb: &mut Framebuffer, label: &str, left: bool) {
    // Frame-relative, not panel-relative: the portrait page's footer sits
    // at the bottom of the upright frame. Landscape frames keep the
    // historical panel numbers.
    let font = display::font::literata_small(FontStyle::Regular);
    let frame_right = fb.frame_width() as i16 - 8;
    let portrait = fb.frame_width() < fb.frame_height();
    let lift = if portrait { PORTRAIT_FOOTER_LIFT } else { 0 };
    let baseline = fb.frame_height() as i16 - 3 - lift;
    let x = if left {
        READER_LEFT_X + 16
    } else {
        let width = measure_text(font, label) as i16;
        frame_right - width - 16
    };
    draw_text(fb, font, label, x, baseline, false);
}

pub const READER_PAGE_TOP: i16 = 6;
/// Footer band top: 14 rows up from the panel's bottom edge. Panel-relative
/// so the X3's taller page pushes the footer to its own bottom edge; on the
/// X4 this is the historical 466.
pub const READER_FOOTER_TOP: i16 = display::HEIGHT as i16 - 14;
/// Page-counter text baseline: as low as it goes without clipping (the
/// slash inks 2 rows below its baseline). Used by `fw::views` so the
/// footer's exact panel-relative position lives in one place instead of
/// being kept in sync by hand across crates.
pub const READER_FOOTER_BASELINE_Y: i16 = display::HEIGHT as i16 - 3;
/// Last permissible baseline row for body ink. Derived, not tuned: the
/// page counter's '/' ink starts 12 rows up from its baseline
/// (`READER_FOOTER_BASELINE_Y`), and the deepest body glyph reaches 7 rows
/// below its baseline (comma-below diacritics), so bottom - 3 - 12 - 7 - 1
/// keeps every possible descender a clear row away from the counter. On the
/// X4 this is the historical 457.
pub const READER_PAGE_BOTTOM: i16 = display::HEIGHT as i16 - 23;
pub const READER_LEFT_X: i16 = 8;
/// Right text margin: 8 rows in from the panel's right edge (the X4's
/// historical 792). Panel-relative so the X3's narrower panel keeps the
/// same inset rather than letting body ink touch the edge.
pub const READER_RIGHT_X: i16 = display::WIDTH as i16 - 8;
pub const READER_WRAP_SAFETY: i16 = 4;
/// Version of the wrap rules and page constants in this module, and of the
/// cached section content keyed off it. Bump when layout changes for
/// unchanged type settings, or when the cache encoding changes so stale
/// sections rebuild. v8: chapters no longer truncate at the text budget and
/// style markers are stored only on run change. v9: intermediate sections
/// end on a whole page (the half-finished page carries into the next
/// section) so chunk seams no longer leave a short, half-empty page. v10:
/// the full chapter list is written to its own TOC.BIN at build time and
/// read on demand for the overview, so a long book's TOC is no longer capped
/// at 128; existing caches rebuild to produce that file. v11: the TOC.BIN
/// chapter-title budget grew 44->60 bytes (record 48->64), so the colophon
/// shows longer chapter names; existing caches rebuild to widen their records.
/// v12: Body paragraphs take a book-style first-line indent instead of an
/// inter-paragraph gap. The indent narrows each paragraph's opening line, so
/// wrap points and the persisted paragraph-start flag change; existing caches
/// rebuild.
/// v13: the Type Weight setting joins the layout config. Heavier (SemiBold)
/// body glyphs are wider than Regular, so wrap points change with weight;
/// existing caches rebuild on a weight change.
/// v14: the Font setting joins the layout config. The second family's advances
/// differ from Literata's at every size, so wrap points change with family;
/// existing caches rebuild on a family change.
/// v15: the second family changed from Bookerly to Merriweather. Its advances
/// differ, but the family bit (1) does not, so a version bump is what retires
/// any pagination cached under the old face. v16: font advances moved to 12.4
/// fixed point and generated kerning tables now affect line widths. v17:
/// family widened from one bit to two bits so the Custom slot cannot collide
/// with the version field. v18: the page box joins the layout config as a
/// portrait bit — the upright frame wraps at the short axis, so portrait
/// and landscape pagination cache separately. v19: the shared page walk fits
/// ink rather than trailing paragraph spacing at the page edge. Retire indexes
/// built by the old firmware walk, including their whole-book page totals.
/// v20: cached text changed — the OPF streams whole (no more chapters lost
/// past 16 KB of package document), spaces between styled runs and `&nbsp;`
/// survive, scene breaks (`<hr>`) render, comments no longer swallow text,
/// and lines no longer break early at section seams. Rebuild every book.
/// v21: front matter before the text start is built into the book (it used
/// to be dropped), which moves every page number after it. v22: books of
/// more than 192 spine items or 320 sections were cached cut short; rebuild
/// them whole. v23: justified prose hyphenates (patterns for English, the
/// book's soft hyphens and compound dashes everywhere), which moves line
/// breaks throughout. v24: a v23 build lost the space at the start of
/// some styled runs and parser chunks; retire those lines. v25: the portrait
/// page ends 8 rows higher, clear of the bezel.
const READER_LAYOUT_VERSION: u16 = 25;

/// Panel-geometry salt folded into the version bits: wrap points and page
/// heights depend on the page box, so pagination cached on one panel must
/// read as stale on the other (an SD card can move between an X4 and an
/// X3). Zero on the X4's 800x480 keeps every existing cache valid; other
/// geometries claim a disjoint version band well clear of routine bumps.
/// (64, not the former 256 or 128: the config field gained the portrait
/// bit and then the alignment bit, so version + salt must stay under 128
/// to fit the u16.)
const PANEL_LAYOUT_SALT: u16 = if display::WIDTH == 800 && display::HEIGHT == 480 {
    0
} else {
    64
};

// Versions stay below the salt so the two panels' bands never meet.
const _: () =
    assert!(READER_LAYOUT_VERSION < 64 && READER_LAYOUT_VERSION + PANEL_LAYOUT_SALT < 128);

/// Section cache layout config: the wrap-rule version plus the layout the
/// section was paginated under. Stored in cache headers; a mismatch on
/// load invalidates the cached pagination and rebuilds it. Bit layout:
/// spacing in bits 0-1 (a spacing change only re-walks heights, so the
/// load check masks these off), size in bits 2-3, weight in bit 4, family
/// in bits 5-6, portrait in bit 7, version in bits 8-14, and left
/// alignment in bit 15 (zero for justified, so caches built before the
/// setting existed stay valid). Size, weight, family, the page box, and
/// alignment all change wrap points, so a change in any forces a full
/// rebuild.
pub fn reader_layout_config(settings: TypeSettings, portrait: bool) -> u16 {
    ((settings.alignment as u16) << 15)
        | ((READER_LAYOUT_VERSION + PANEL_LAYOUT_SALT) << 8)
        | ((portrait as u16) << 7)
        | ((settings.family as u16) << 5)
        | ((settings.weight as u16) << 4)
        | ((settings.size as u16) << 2)
        | settings.spacing as u16
}

/// The alignment a block lays out and draws with: text the book sets
/// justified follows the Alignment setting; every other alignment is the
/// book's own.
pub fn effective_align(align: TextAlign, settings: TypeSettings) -> TextAlign {
    match (align, settings.alignment) {
        (TextAlign::Justify, Alignment::Left) => TextAlign::Left,
        _ => align,
    }
}

/// The reading body face for the given settings and style run.
pub fn body_font(settings: TypeSettings, style: FontStyle) -> &'static BitmapFont {
    family_weighted(settings.family, settings.size, settings.weight, style)
}

/// Baseline-to-baseline advance. Body values per (size, spacing); H1/H2
/// carry extra lead. Medium/Normal runs 26 (130% leading) so the default
/// page grid closes at seventeen lines: 6 + 17*26 = 448 <= 457, where the
/// historical 27 left a dead row above the footer on every full page.
pub fn line_advance(settings: TypeSettings, role: TextRole) -> i16 {
    let body = body_line_advance(settings.size, settings.spacing);
    if matches!(role, TextRole::Heading1 | TextRole::Heading2) {
        body + 5
    } else {
        body
    }
}

/// A body line's advance: [`line_advance`] for `TextRole::Body`.
const fn body_line_advance(size: FontSize, spacing: LineSpacing) -> i16 {
    match (size, spacing) {
        (FontSize::Small, LineSpacing::Compact) => 22,
        (FontSize::Small, LineSpacing::Normal) => 24,
        (FontSize::Small, LineSpacing::Relaxed) => 28,
        (FontSize::Medium, LineSpacing::Compact) => 25,
        (FontSize::Medium, LineSpacing::Normal) => 26,
        (FontSize::Medium, LineSpacing::Relaxed) => 31,
        (FontSize::Large, LineSpacing::Compact) => 29,
        (FontSize::Large, LineSpacing::Normal) => 32,
        (FontSize::Large, LineSpacing::Relaxed) => 36,
    }
}

pub fn paragraph_gap(role: TextRole) -> i16 {
    match role {
        TextRole::Heading1 | TextRole::Heading2 => 10,
        TextRole::Heading3 => 6,
        TextRole::BlockQuote => 6,
        // Body paragraphs separate by their first-line indent, book-style,
        // rather than an inter-paragraph gap: the two together read as
        // redundant. See [`paragraph_indent`].
        TextRole::Body => 0,
    }
}

/// First-line paragraph indent, in pixels, scaled to the body size. This is
/// the book-style paragraph cue that replaces the inter-paragraph gap for
/// Body text (see [`paragraph_gap`]). Roughly 1.3em at each size.
pub fn paragraph_indent(size: FontSize) -> i16 {
    match size {
        FontSize::Small => 24,
        FontSize::Medium => 28,
        FontSize::Large => 34,
    }
}

/// The first-line indent block `index` draws with, or 0 when it takes none:
/// non-Body roles, centered text, and continuation lines of a paragraph that
/// wrapped or carried across a section boundary all stay flush left.
pub fn block_first_line_indent(source: &impl ReadingBlocks, index: usize) -> i16 {
    let Some(record) = source.block(index) else {
        return 0;
    };
    if matches!(record.role, TextRole::Body)
        && matches!(record.align, TextAlign::Left | TextAlign::Justify)
        && source.paragraph_start(index)
    {
        paragraph_indent(source.type_settings().size)
    } else {
        0
    }
}

pub fn reader_x_for(role: TextRole) -> i16 {
    PageBox::LANDSCAPE.x_for(role)
}

/// Running ink measurement: pen advance plus the rightmost inked edge,
/// which can exceed the advance for glyphs whose bitmap overhangs their
/// advance width (italics, some punctuation).
#[derive(Clone, Copy, Debug, Default)]
pub struct InkCursor {
    advance_fp: i32,
    right: i16,
    previous: Option<u16>,
}

impl InkCursor {
    pub const fn new() -> Self {
        Self {
            advance_fp: 0,
            right: 0,
            previous: None,
        }
    }

    #[inline]
    pub fn push_char(&mut self, font: &BitmapFont, ch: char) {
        let codepoint = if ch as u32 > u16::MAX as u32 {
            b'?' as u16
        } else {
            ch as u16
        };
        let Some((metric, _)) = font.glyph(codepoint).or_else(|| font.glyph(b'?' as u16)) else {
            self.advance_fp += 8 << 4;
            self.right = self.right.max(fixed_ceil(self.advance_fp));
            self.previous = Some(b'?' as u16);
            return;
        };
        let drawn = if font.glyph(codepoint).is_some() {
            codepoint
        } else {
            b'?' as u16
        };
        if let Some(left) = self.previous {
            self.advance_fp += font.kerning_adjust_fp(left, drawn) as i32;
        }
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
        self.previous = Some(drawn);
    }

    #[inline]
    pub fn reset_pair(&mut self) {
        self.previous = None;
    }

    /// An inline image `width` columns wide: it takes exactly its width
    /// and kerns with nothing.
    pub fn push_object(&mut self, width: u16) {
        let left = fixed_round(self.advance_fp);
        self.right = self.right.max(left.saturating_add(width as i16));
        self.advance_fp += i32::from(width) << 4;
        self.previous = None;
    }

    pub fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}

pub fn text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
    let mut ink = InkCursor::new();
    for ch in text.chars() {
        ink.push_char(font, ch);
    }
    ink.width()
}

/// Incremental ink measurement over cached styled text: [`STYLE_MARKER`]
/// followed by a style digit switches the active font mid-stream, staying
/// inside one type size. `Copy`, so callers can checkpoint before a word
/// and roll back on overflow.
#[derive(Clone, Copy)]
pub struct StyledInkCursor {
    ink: InkCursor,
    settings: TypeSettings,
    font: &'static BitmapFont,
}

impl StyledInkCursor {
    pub fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            ink: InkCursor::new(),
            settings,
            font: body_font(settings, default_style),
        }
    }

    /// A [`STYLE_MARKER`] and its code digit must arrive within one
    /// fragment; a marker split across fragments loses its style switch.
    pub fn push_str(&mut self, text: &str) {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    self.ink.reset_pair();
                    self.font = body_font(
                        self.settings,
                        style_from_marker_code(code).unwrap_or(FontStyle::Regular),
                    );
                }
                continue;
            }
            self.ink.push_char(self.font, ch);
        }
    }

    /// An inline image `width` columns wide. The style in effect carries
    /// on past it.
    pub fn push_object(&mut self, width: u16) {
        self.ink.push_object(width);
    }

    pub fn width(&self) -> i16 {
        self.ink.width()
    }
}

pub fn styled_text_ink_width(text: &str, settings: TypeSettings, default_style: FontStyle) -> i16 {
    let mut cursor = StyledInkCursor::new(settings, default_style);
    cursor.push_str(text);
    cursor.width()
}

/// The style a stored line opens in: what its text up to the first
/// [`STYLE_MARKER`] draws and measures in.
///
/// The line builder starts every line in Regular and marks a run only where
/// the style changes, so a line led by a marker opens in that marker's
/// style, and any other line that carries markers opens in Regular: in
/// `plain {1}italic {0}end.` (`{n}` standing for a marker and its style
/// code) the lead-in is plain. Only a line without
/// markers draws whole in `block_style`. Caches built before this rule
/// stored a line's first marker as its block style, which for that line
/// is Italic; deriving the opening from the text draws them right too.
pub fn line_opening_style(text: &str, block_style: FontStyle) -> FontStyle {
    let mut chars = text.chars();
    if chars.next() == Some(STYLE_MARKER) {
        return chars
            .next()
            .and_then(style_from_marker_code)
            .unwrap_or(FontStyle::Regular);
    }
    if text.contains(STYLE_MARKER) {
        FontStyle::Regular
    } else {
        block_style
    }
}

/// Greedy word wrap step. Starting at `cursor` (skipping leading ASCII
/// whitespace), returns `(line_start, line_end, next_cursor)` for the
/// longest run of words whose ink width fits `x..max_x`, or a single
/// overlong word when nothing fits.
pub fn next_wrapped_line(
    text: &str,
    mut cursor: usize,
    font: &'static BitmapFont,
    x: i16,
    max_x: i16,
) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }

    let mut ink = InkCursor::new();
    let mut measured_to = cursor;
    let mut scan = cursor;
    let mut best_end = cursor;
    let mut best_next = cursor;
    while scan < bytes.len() {
        let word_start = scan;
        while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        let word_end = scan;
        for ch in text[measured_to..word_end].chars() {
            ink.push_char(font, ch);
        }
        measured_to = word_end;
        if x + ink.width() + READER_WRAP_SAFETY > max_x {
            if best_end == cursor {
                return Some((word_start, word_end, word_end));
            }
            return Some((cursor, best_end, best_next));
        }
        best_end = word_end;
        while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
            scan += 1;
        }
        best_next = scan;
    }

    Some((cursor, best_end, best_next.max(best_end)))
}

pub fn wrapped_line_count(
    font: &'static BitmapFont,
    text: &str,
    max_width: i16,
    first_indent: i16,
) -> u16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut lines = 0u16;
    // The opening line starts `first_indent` in, so it holds fewer words;
    // every following line runs the full width.
    let mut x = first_indent;
    while let Some((_, _, next_cursor)) = next_wrapped_line(text, cursor, font, x, max_width) {
        lines = lines.saturating_add(1);
        x = 0;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)]
pub fn wrapped_block_height(
    font: &'static BitmapFont,
    text: &str,
    role: TextRole,
    align: TextAlign,
    line_advance: i16,
    first_indent: i16,
    page_box: PageBox,
) -> i16 {
    let max_width = page_box.right
        - if align == TextAlign::Center {
            page_box.left
        } else {
            page_box.x_for(role)
        };
    wrapped_line_count(font, text, max_width, first_indent).max(1) as i16 * line_advance
}

pub fn draw_styled_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    text: &str,
    x: i16,
    baseline_y: i16,
    default_style: FontStyle,
) -> i16 {
    let mut cursor_x = x;
    let mut run_start = 0usize;
    let mut style = default_style;
    let mut iter = text.char_indices().peekable();
    while let Some((index, ch)) = iter.next() {
        if ch != STYLE_MARKER {
            continue;
        }
        if run_start < index {
            cursor_x = draw_text(
                fb,
                body_font(settings, style),
                &text[run_start..index],
                cursor_x,
                baseline_y,
                false,
            );
        }
        if let Some((code_index, code)) = iter.next() {
            style = style_from_marker_code(code).unwrap_or(style);
            run_start = code_index + code.len_utf8();
        } else {
            run_start = index + ch.len_utf8();
        }
    }
    if run_start < text.len() {
        cursor_x = draw_text(
            fb,
            body_font(settings, style),
            &text[run_start..],
            cursor_x,
            baseline_y,
            false,
        );
    }
    cursor_x
}

/// Lines in a row that may end in a hyphen before the line builder holds
/// out for a looser line instead (a "ladder" of hyphens down the margin).
pub const MAX_HYPHEN_RUN: u8 = 2;

/// Whether the word that no longer fits a justified line should be split
/// rather than moved whole to the next line. `slack` is the width the line
/// has left without it, `gaps` its inter-word spaces, `space` a normal word
/// space, and `run` how many lines in a row already end in a hyphen.
///
/// Moving the word is preferred while it costs each gap at most half a
/// space; past that, the line would look loose and a break is worth a
/// hyphen. After `MAX_HYPHEN_RUN` hyphenated lines in a row it takes a
/// gap doubling to justify another. A line holding a single word cannot
/// stretch at all, so it always tries.
pub fn worth_hyphenating(slack: i16, gaps: usize, space: i16, run: u8) -> bool {
    if gaps == 0 {
        return true;
    }
    let slack = i32::from(slack);
    let spaces = gaps as i32 * i32::from(space.max(1));
    if run >= MAX_HYPHEN_RUN {
        slack > spaces
    } else {
        slack * 2 > spaces
    }
}

/// A normal word space in the reading face, for the spacing rules above.
pub fn word_space_width(settings: TypeSettings, style: FontStyle) -> i16 {
    measure_text(body_font(settings, style), " ") as i16
}

/// Extra width to spread across a justified line's inter-word gaps, or
/// `None` when it should keep natural spacing: no gaps, nothing to add, a
/// line so short (under two thirds of the measure) that stretching it
/// would open rivers, or one whose gaps would each grow by more than one
/// and a half spaces -- typically a line cut short by a word that could
/// not be broken.
pub fn justify_extra(
    x: i16,
    natural_width: i16,
    right: i16,
    gaps: usize,
    space: i16,
) -> Option<i16> {
    let target = right - READER_WRAP_SAFETY;
    let extra = target - (x + natural_width);
    if gaps == 0
        || extra <= 0
        || extra * 3 > target - x
        || i32::from(extra) * 2 > gaps as i32 * i32::from(space) * 3
    {
        return None;
    }
    Some(extra)
}

/// [`justify_extra`] for a line holding inline images: justified whenever
/// it fills two thirds of the measure, however far its gaps stretch, as a
/// browser justifies. An image takes width but adds no gap, so such a line
/// has few gaps for its measure; a drop cap opening a paragraph is where
/// this shows, when the word after the line will not fit beside the
/// initial and the first line would otherwise stay ragged among justified
/// ones.
pub fn justify_inline_extra(x: i16, natural_width: i16, right: i16, gaps: usize) -> Option<i16> {
    let target = right - READER_WRAP_SAFETY;
    let extra = target - (x + natural_width);
    if gaps == 0 || extra <= 0 || extra * 3 > target - x {
        return None;
    }
    Some(extra)
}

/// Inter-word gaps in a stored line: ASCII spaces before more text. A
/// no-break space is part of its word and never stretches.
pub fn justify_gap_count(text: &str) -> usize {
    text.as_bytes()
        .windows(2)
        .filter(|pair| pair[0] == b' ' && pair[1] != b' ')
        .count()
}

/// One step of a styled line walked for justified drawing.
pub enum StyledWordStep<'a> {
    /// A run of non-space text (style markers included) and the style in
    /// effect at its start.
    Word(&'a str, FontStyle),
    /// An inter-word space and the style in effect there.
    Gap(FontStyle),
}

/// Walk a styled line word by word for justified drawing. Shared by the
/// built-in and SD custom-font drawers so both justify the same way. Stops
/// early (returning false) when `visit` does.
pub fn walk_styled_words(
    text: &str,
    default_style: FontStyle,
    mut visit: impl FnMut(StyledWordStep<'_>) -> bool,
) -> bool {
    let mut style = default_style;
    for (index, segment) in text.split(' ').enumerate() {
        if segment.is_empty() {
            continue;
        }
        if index > 0 && !visit(StyledWordStep::Gap(style)) {
            return false;
        }
        if !visit(StyledWordStep::Word(segment, style)) {
            return false;
        }
        let mut chars = segment.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                if let Some(code) = chars.next() {
                    style = style_from_marker_code(code).unwrap_or(style);
                }
            }
        }
    }
    true
}

/// Draw one stored line justified to `right`: the leftover width is spread
/// across its inter-word spaces (earlier gaps take the remainder pixels).
/// Falls back to natural spacing when `justify_extra` declines.
pub fn draw_justified_styled_line(
    fb: &mut Framebuffer,
    settings: TypeSettings,
    text: &str,
    x: i16,
    baseline_y: i16,
    right: i16,
    default_style: FontStyle,
) -> i16 {
    let gaps = justify_gap_count(text);
    let natural = styled_text_ink_width(text, settings, default_style);
    let space = word_space_width(settings, default_style);
    let Some(extra) = justify_extra(x, natural, right, gaps, space) else {
        return draw_styled_line(fb, settings, text, x, baseline_y, default_style);
    };
    let per_gap = extra / gaps as i16;
    let mut remainder = extra % gaps as i16;
    let mut cursor_x = x;
    walk_styled_words(text, default_style, |step| {
        match step {
            StyledWordStep::Word(word, style) => {
                cursor_x = draw_styled_line(fb, settings, word, cursor_x, baseline_y, style);
            }
            StyledWordStep::Gap(style) => {
                cursor_x += measure_text(body_font(settings, style), " ") as i16 + per_gap;
                if remainder > 0 {
                    cursor_x += 1;
                    remainder -= 1;
                }
            }
        }
        true
    });
    cursor_x
}

pub fn draw_centered_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    mut baseline_y: i16,
    max_width: i16,
    line_advance: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, 0, max_width)
    {
        let line = &text[line_start..line_end];
        let width = text_ink_width(font, line).min(max_width);
        let x = ((fb.frame_width() as i16 - width) / 2).max(20);
        draw_text(fb, font, line, x, baseline_y, false);
        baseline_y += line_advance;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

#[allow(clippy::too_many_arguments)]
pub fn draw_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
    first_indent: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut line_x = x + first_indent;
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, line_x, max_x)
    {
        draw_text(
            fb,
            font,
            &text[line_start..line_end],
            line_x,
            baseline_y,
            false,
        );
        baseline_y += line_advance;
        line_x = x;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

#[allow(clippy::too_many_arguments)]
pub fn draw_justified_wrapped_literata(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    text: &str,
    x: i16,
    mut baseline_y: i16,
    max_x: i16,
    line_advance: i16,
    first_indent: i16,
) -> i16 {
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    let mut line_x = x + first_indent;
    while let Some((line_start, line_end, next_cursor)) =
        next_wrapped_line(text, cursor, font, line_x, max_x)
    {
        let is_last_line = next_cursor >= bytes.len();
        draw_justified_line(
            fb,
            font,
            &text[line_start..line_end],
            line_x,
            baseline_y,
            max_x,
            is_last_line,
        );
        baseline_y += line_advance;
        line_x = x;
        cursor = next_cursor;
        if cursor >= bytes.len() {
            break;
        }
    }

    baseline_y
}

fn draw_justified_line(
    fb: &mut Framebuffer,
    font: &'static BitmapFont,
    line: &str,
    x: i16,
    baseline_y: i16,
    max_x: i16,
    is_last_line: bool,
) {
    let gap_count = line
        .as_bytes()
        .windows(2)
        .filter(|pair| pair[0] == b' ' && pair[1] != b' ')
        .count();
    if is_last_line || gap_count == 0 {
        draw_text(fb, font, line, x, baseline_y, false);
        return;
    }

    let text_width = text_ink_width(font, line);
    let extra = (max_x - x - READER_WRAP_SAFETY - text_width).max(0);
    let extra_per_gap = extra / gap_count as i16;
    let mut remainder = extra % gap_count as i16;
    let mut cursor_x = x;
    let mut word_start = None;

    for (index, byte) in line.bytes().enumerate() {
        if byte == b' ' {
            if let Some(start) = word_start.take() {
                let word = &line[start..index];
                cursor_x = draw_text(fb, font, word, cursor_x, baseline_y, false);
            }
            let mut gap = measure_text(font, " ") as i16 + extra_per_gap;
            if remainder > 0 {
                gap += 1;
                remainder -= 1;
            }
            cursor_x += gap;
        } else if word_start.is_none() {
            word_start = Some(index);
        }
    }
    if let Some(start) = word_start {
        draw_text(fb, font, &line[start..], cursor_x, baseline_y, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use display::font::{style_marker_code, FontFamily, FontWeight};

    /// Minimal blocks for exercising the indent predicate: roles, aligns, and
    /// paragraph-end flags, with the default `paragraph_start` derivation.
    struct IndentBlocks<'a> {
        roles: &'a [TextRole],
        aligns: &'a [TextAlign],
        ends: &'a [bool],
    }

    impl ReadingBlocks for IndentBlocks<'_> {
        fn block_count(&self) -> usize {
            self.roles.len()
        }
        fn block(&self, index: usize) -> Option<BlockRecord> {
            Some(BlockRecord {
                text_offset: 0,
                text_len: 0,
                line_count: 1,
                role: *self.roles.get(index)?,
                style: proto::text::FontStyle::Regular,
                align: self.aligns[index],
                image_height: 0,
            })
        }
        fn block_text(&self, _index: usize) -> &str {
            ""
        }
        fn block_style(&self, _index: usize) -> FontStyle {
            FontStyle::Regular
        }
        fn page_break_before(&self, _index: usize) -> bool {
            false
        }
        fn paragraph_end(&self, index: usize) -> bool {
            self.ends.get(index).copied().unwrap_or(true)
        }
    }

    /// Lines of body text with images among them: `Some((width, height))`
    /// marks an image block, whose text is its token.
    struct ImageBlocks {
        blocks: Vec<(BlockRecord, String)>,
        settings: TypeSettings,
        page_box: PageBox,
        /// Paragraph ends, block by block; every block ends one when empty.
        ends: Vec<bool>,
        /// Paragraph starts, when not derived from the ends.
        starts: Option<Vec<bool>>,
    }

    impl ImageBlocks {
        fn new(layout: &[Option<(u16, u16)>], settings: TypeSettings, page_box: PageBox) -> Self {
            let blocks = layout
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    let mut record = BlockRecord {
                        text_offset: 0,
                        text_len: 0,
                        line_count: 1,
                        role: TextRole::Body,
                        style: proto::text::FontStyle::Regular,
                        align: TextAlign::Left,
                        image_height: 0,
                    };
                    let text = match image {
                        Some((width, height)) => {
                            record.align = TextAlign::Center;
                            record.image_height = *height;
                            let mut out = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
                            proto::image::token::encode_image_token(
                                index as u16,
                                *width,
                                Some("A figure"),
                                &mut out,
                            )
                            .unwrap()
                            .into()
                        }
                        None => "Typography gjpqy".into(),
                    };
                    (record, text)
                })
                .collect();
            Self {
                blocks,
                settings,
                page_box,
                ends: Vec::new(),
                starts: None,
            }
        }
    }

    impl ReadingBlocks for ImageBlocks {
        fn block_count(&self) -> usize {
            self.blocks.len()
        }
        fn block(&self, index: usize) -> Option<BlockRecord> {
            self.blocks.get(index).map(|block| block.0)
        }
        fn block_text(&self, index: usize) -> &str {
            self.blocks.get(index).map_or("", |block| block.1.as_str())
        }
        fn block_style(&self, _index: usize) -> FontStyle {
            FontStyle::Regular
        }
        fn page_break_before(&self, _index: usize) -> bool {
            false
        }
        fn paragraph_end(&self, index: usize) -> bool {
            self.ends.get(index).copied().unwrap_or(true)
        }
        fn paragraph_start(&self, index: usize) -> bool {
            match &self.starts {
                Some(starts) => starts.get(index).copied().unwrap_or(false),
                None => index == 0 || self.paragraph_end(index - 1),
            }
        }
        fn type_settings(&self) -> TypeSettings {
            self.settings
        }
        fn page_box(&self) -> PageBox {
            self.page_box
        }
    }

    fn every_body_type() -> impl Iterator<Item = TypeSettings> {
        [FontFamily::Literata, FontFamily::Merriweather]
            .into_iter()
            .flat_map(|family| {
                [FontSize::Small, FontSize::Medium, FontSize::Large]
                    .into_iter()
                    .flat_map(move |size| {
                        [FontWeight::Normal, FontWeight::Heavy]
                            .into_iter()
                            .flat_map(move |weight| {
                                [
                                    LineSpacing::Compact,
                                    LineSpacing::Normal,
                                    LineSpacing::Relaxed,
                                ]
                                .into_iter()
                                .map(move |spacing| TypeSettings {
                                    family,
                                    size,
                                    weight,
                                    spacing,
                                    ..TypeSettings::DEFAULT
                                })
                            })
                    })
            })
    }

    /// A text line, an image the full column wide, and another line: the
    /// first line's baseline, the image's rows, the second line's baseline.
    fn line_image_line(settings: TypeSettings, text: &str) -> (i16, (i16, i16), i16, ImageBlocks) {
        let page_box = PageBox::LANDSCAPE;
        let column = (page_box.right - page_box.left) as u16;
        let mut source = ImageBlocks::new(&[None, Some((column, 40)), None], settings, page_box);
        for block in [0, 2] {
            source.blocks[block].1 = text.into();
        }
        let page = page_record_at(&source, 0);
        assert_eq!(page.block_count, 3, "{settings:?}");
        let mut baselines = Vec::new();
        let mut rows = None;
        for_each_drawable_block(&source, page, |block| {
            match block.image {
                Some(image) => rows = Some((image.y, image.y + image.height as i16)),
                None => baselines.push(block.y),
            }
            true
        });
        (baselines[0], rows.expect("an image"), baselines[1], source)
    }

    #[test]
    fn every_glyph_clears_an_image_by_a_row_above_and_below() {
        for settings in every_body_type() {
            // The gaps are what the type needs, never cut short by a clamp.
            let font = body_font(settings, FontStyle::Regular);
            let descent = i16::from(font.line_height) - i16::from(font.baseline);
            assert_eq!(image_top_gap(settings), descent + 1, "{settings:?}");
            let reach = tallest_ink_above_baseline(settings.size) + 1;
            let advance = line_advance(settings, TextRole::Body);
            assert_eq!(
                image_bottom_gap(settings),
                (reach - advance).max(0),
                "{settings:?}"
            );

            let (above, (top, end), below, _) = line_image_line(settings, "line");
            let mut tallest = 0;
            for style in STYLES {
                let font = body_font(settings, style);
                for (index, metric) in font.metrics.iter().enumerate() {
                    let codepoint = font.codepoints[index];
                    // A glyph inks rows `baseline + y_offset` through
                    // `baseline + y_offset + height - 1`.
                    let first = i16::from(metric.y_offset);
                    let last = first + i16::from(metric.height) - 1;
                    assert!(
                        above + last < top - 1,
                        "{settings:?} {style:?} U+{codepoint:04X} reaches row {} above an image at {top}",
                        above + last
                    );
                    assert!(
                        below + first > end,
                        "{settings:?} {style:?} U+{codepoint:04X} reaches row {} below an image ending at {end}",
                        below + first
                    );
                    tallest = tallest.max(-first);
                }
            }
            assert_eq!(
                tallest,
                tallest_ink_above_baseline(settings.size),
                "{settings:?}"
            );
        }
    }

    #[test]
    fn stacked_marks_and_descenders_leave_a_clear_row_on_the_page() {
        // The tallest glyphs (stacked marks, accented capitals) and the
        // deepest (marks below, descenders), in every style.
        let mut text = String::new();
        for style in STYLES {
            text.push(STYLE_MARKER);
            text.push(style_marker_code(style));
            text.push_str("\u{1EB2}\u{1E4C}\u{C9}\u{C5}\u{122}\u{1E00}\u{104}gjpqy ");
        }
        let text = text.trim_end();
        for settings in every_body_type() {
            let (_, (top, end), _, source) = line_image_line(settings, text);
            let mut fb = Framebuffer::new();
            draw_reading_page_body(&mut fb, &source, page_record_at(&source, 0), |_, _| {});
            let inked = |y: i16| (0..fb.frame_width()).any(|x| !fb.pixel(x, y as usize));
            // A clear row above, the image's rows, a clear row below.
            for y in top - 1..=end {
                assert!(
                    !inked(y),
                    "{settings:?}: text ink at row {y}, image {top}..{end}"
                );
            }
            // And the text comes close: these glyphs are what the gaps are
            // for. At Relaxed spacing the line below sits further off.
            assert!(
                (top - 4..top - 1).any(inked),
                "{settings:?}: descenders above"
            );
            if settings.spacing != LineSpacing::Relaxed {
                assert!((end + 1..end + 4).any(inked), "{settings:?}: marks below");
            }
        }
    }

    #[test]
    fn fitted_images_fit_a_page_with_their_gap_and_never_grow() {
        for page_box in [PageBox::LANDSCAPE, PageBox::PORTRAIT] {
            let column = (page_box.right - page_box.left) as u16;
            let rows = (page_box.bottom - page_box.top) as u16;
            // Small images keep their size.
            assert_eq!(fit_image(page_box, 138, 40), (138, 40));
            for (src_w, src_h) in [(640, 900), (1600, 1200), (5000, 10), (10, 5000), (1, 1)] {
                let (width, height) = fit_image(page_box, src_w, src_h);
                assert!(width <= column && u32::from(width) <= src_w);
                assert!(u32::from(height) <= src_h);
                assert!(height as i16 + IMAGE_GAPS_MAX <= rows as i16);
            }
        }
    }

    #[test]
    fn image_blocks_take_their_gap_and_height_and_never_stall_pagination() {
        for settings in every_body_type() {
            for page_box in [PageBox::LANDSCAPE, PageBox::PORTRAIT] {
                let (width, height) = fit_image(page_box, 640, 5000);
                let source = ImageBlocks::new(
                    &[
                        None,
                        Some((width, height)),
                        None,
                        Some((width, height)),
                        None,
                    ],
                    settings,
                    page_box,
                );
                assert_eq!(
                    block_height(&source, 1),
                    image_top_gap(settings) + height as i16 + image_bottom_gap(settings)
                );
                // A page-tall image cannot share a page with a line, and
                // each page still holds at least one block.
                let pages: Vec<_> = (0..5).map(|page| page_record_at(&source, page)).collect();
                assert_eq!(paginate_block_pages(&source), 5, "{settings:?}");
                assert!(pages.iter().all(|page| page.block_count == 1));
            }
        }
    }

    #[test]
    fn a_damaged_image_height_stays_within_a_page() {
        let page_box = PageBox::LANDSCAPE;
        // Heights from 0x8000 up mark a line with inline images, whose own
        // damage `damaged_inline_heights_stay_within_the_cap` covers.
        let source = ImageBlocks::new(
            &[Some((100, 0x7FFF)), None, Some((100, 30_000)), None],
            TypeSettings::DEFAULT,
            page_box,
        );
        assert_eq!(paginate_block_pages(&source), 4);
        for page in 0..4 {
            let mut blocks = 0;
            for_each_drawable_block(&source, page_record_at(&source, page), |block| {
                if let Some(image) = block.image {
                    assert!(image.y >= page_box.top);
                    assert!(image.y + image.height as i16 <= page_box.bottom);
                }
                blocks += 1;
                true
            });
            assert_eq!(blocks, 1);
        }
    }

    #[test]
    fn only_pages_holding_an_image_block_have_images() {
        let source = ImageBlocks::new(
            &[None, None, Some((300, 400)), None],
            TypeSettings::DEFAULT,
            PageBox::LANDSCAPE,
        );
        let page = |first_block, block_count| PageRecord {
            first_block,
            block_count,
        };
        assert!(!page_has_images(&source, page(0, 2)));
        assert!(page_has_images(&source, page(0, 3)));
        assert!(page_has_images(&source, page(2, 1)));
        assert!(!page_has_images(&source, page(3, 1)));
        // A record past the blocks there are reads as none.
        assert!(!page_has_images(&source, page(3, 40)));
        assert!(!page_has_images(&source, page(0, 0)));
    }

    #[test]
    fn image_rects_sit_below_the_gap_centred_by_their_token_width() {
        let settings = TypeSettings::DEFAULT;
        let page_box = PageBox::LANDSCAPE;
        let source = ImageBlocks::new(&[None, Some((300, 120)), None], settings, page_box);
        let page = page_record_at(&source, 0);
        assert_eq!(page.block_count, 3);
        let mut seen = Vec::new();
        for_each_drawable_block(&source, page, |block| {
            let image = block.image.map(|image| {
                (
                    image.x,
                    image.y,
                    image.width,
                    image.height,
                    image.slot,
                    String::from(image.alt),
                )
            });
            seen.push((block.y, image));
            true
        });
        let advance = line_advance(settings, TextRole::Body);
        assert_eq!((&seen[0].1, &seen[2].1), (&None, &None));
        let (x, y, width, height, slot, alt) = seen[1].1.clone().expect("block 1 is an image");
        // The line above has its baseline where the image block begins.
        assert_eq!(seen[0].0, page_box.top + advance);
        assert_eq!(y, seen[0].0 + image_top_gap(settings));
        assert_eq!((width, height), (300, 120));
        assert_eq!(
            x,
            page_box.left + (page_box.right - page_box.left - 300) / 2
        );
        assert_eq!((slot, alt.as_str()), (Some(1), "A figure"));
        // The next line's slot starts below the image and its bottom gap.
        assert_eq!(seen[2].0, y + 120 + image_bottom_gap(settings) + advance);
    }

    #[test]
    fn the_page_body_hands_image_blocks_to_the_drawer_only() {
        let settings = TypeSettings::DEFAULT;
        let page_box = PageBox::LANDSCAPE;
        let source = ImageBlocks::new(&[None, Some((200, 60))], settings, page_box);
        let page = page_record_at(&source, 0);
        let mut drawn = Vec::new();
        let mut fb = Framebuffer::new();
        fb.set_frame(display::fb::FbFrame::Landscape);
        draw_reading_page_body(&mut fb, &source, page, |_, image| {
            drawn.push((image.x, image.y, image.slot));
        });
        assert_eq!(drawn.len(), 1);
        let (image_x, image_y) = (drawn[0].0, drawn[0].1);
        assert_eq!(drawn[0].2, Some(1));
        // The token's text was not drawn: the image's rows are blank, and
        // the line above's descenders stop short of them.
        for y in image_y..image_y + 60 {
            for x in page_box.left..page_box.right {
                assert!(fb.pixel(x as usize, y as usize), "ink at {x},{y}");
            }
        }
        draw_image_placeholder(
            &mut fb,
            &ReaderImage {
                x: image_x,
                y: image_y,
                width: 200,
                height: 60,
                slot: Some(1),
                alt: "A figure",
                alt_font: body_font(settings, FontStyle::Italic),
            },
        );
        // The placeholder outlines the rect and writes the alt inside it,
        // and nothing else (the token's text) was drawn.
        let (x0, y0) = (image_x as usize, image_y as usize);
        let (x1, y1) = (x0 + 199, y0 + 59);
        assert!(!fb.pixel(x0, y0) && !fb.pixel(x1, y1) && !fb.pixel(x0, y1));
        let inside_ink = (y0 + 1..y1)
            .flat_map(|y| (x0 + 1..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| !fb.pixel(x, y))
            .count();
        assert!(inside_ink > 50, "alt text drawn inside");
        let outside_ink = (y0 - 2..y1 + 20)
            .flat_map(|y| (0..fb.frame_width()).map(move |x| (x, y)))
            .filter(|&(x, y)| (x < x0 || x > x1 || y < y0 || y > y1) && !fb.pixel(x, y))
            .count();
        assert_eq!(outside_ink, 0, "nothing beside or below the box");
    }

    /// A line's text with `{n}` standing for inline image `n` of `images`.
    fn inline_text(template: &str, images: &[(u16, u16, u16)]) -> String {
        let mut text = String::from(template);
        for (index, &(slot, width, height)) in images.iter().enumerate() {
            let mut out = [0u8; proto::image::token::INLINE_IMAGE_TOKEN_BYTES];
            let token = proto::image::token::encode_inline_image_token(
                InlineImageToken {
                    slot,
                    width,
                    height,
                },
                &mut out,
            );
            text = text.replace(&format!("{{{index}}}"), token);
        }
        text
    }

    /// Body lines, some holding inline images: each line's text, alignment,
    /// whether it ends its paragraph, and its tallest image (0 for none).
    fn inline_blocks(
        lines: &[(String, TextAlign, bool, u16)],
        settings: TypeSettings,
        page_box: PageBox,
    ) -> ImageBlocks {
        let mut source = ImageBlocks::new(&vec![None; lines.len()], settings, page_box);
        for (block, (text, align, _, tallest)) in source.blocks.iter_mut().zip(lines) {
            block.0.align = *align;
            if *tallest > 0 {
                block.0.image_height = BlockRecord::inline_image_field(*tallest);
            }
            block.1 = text.clone();
        }
        source.ends = lines.iter().map(|line| line.2).collect();
        source
    }

    #[test]
    fn inline_images_fit_the_cap_and_the_narrowest_line() {
        for page_box in [PageBox::LANDSCAPE, PageBox::PORTRAIT] {
            let narrowest = (page_box.right - page_box.left - 34 - READER_WRAP_SAFETY) as u16;
            // Euclid's symbol keeps its size; Alice's drop cap is capped.
            assert_eq!(fit_inline_image(page_box, 17, 12), (17, 12));
            assert_eq!(fit_inline_image(page_box, 75, 75), (64, 64));
            assert_eq!(fit_inline_image(page_box, 14, 31), (14, 31));
            // A banner is held to the narrowest line.
            let (width, height) = fit_inline_image(page_box, 4000, 60);
            assert_eq!(width, narrowest);
            assert!(height >= 1 && height < 60);
            for (src_w, src_h) in [(1, 1), (640, 900), (64, 64), (65, 65), (5000, 3), (3, 5000)] {
                let (width, height) = fit_inline_image(page_box, src_w, src_h);
                assert!(height <= INLINE_IMAGE_MAX_HEIGHT && width <= narrowest);
                assert!(u32::from(width) <= src_w && u32::from(height) <= src_h);
                assert!(width >= 1 && height >= 1);
                // Never larger than the image would be alone in a paragraph.
                let (block_w, block_h) = fit_image(page_box, src_w, src_h);
                assert!(width <= block_w && height <= block_h);
            }
        }
    }

    /// A text line, a line with an image `height` rows tall inside it, and
    /// another text line: their baselines and the image's rows.
    fn text_inline_text(
        settings: TypeSettings,
        text: &str,
        height: u16,
    ) -> ([i16; 3], (i16, i16), ImageBlocks) {
        let line = inline_text(&format!("{text} {{0}} {text}"), &[(9, 20, height)]);
        let source = inline_blocks(
            &[
                (text.into(), TextAlign::Left, false, 0),
                (line, TextAlign::Left, false, height),
                (text.into(), TextAlign::Left, true, 0),
            ],
            settings,
            PageBox::LANDSCAPE,
        );
        let page = page_record_at(&source, 0);
        assert_eq!(page.block_count, 3, "{settings:?}");
        let mut baselines = [0; 3];
        let mut index = 0;
        for_each_drawable_block(&source, page, |block| {
            baselines[index] = block.y;
            index += 1;
            true
        });
        let mut rows = None;
        let mut fb = Framebuffer::new();
        draw_reading_page_body(&mut fb, &source, page, |_, image| {
            rows = Some((image.y, image.y + image.height as i16));
        });
        (baselines, rows.expect("an image"), source)
    }

    #[test]
    fn every_glyph_clears_an_inline_image_by_a_row_above_and_below() {
        for settings in every_body_type() {
            let advance = line_advance(settings, TextRole::Body);
            for height in [1, 12, 20, 31, 48, 64] {
                let (baselines, (top, end), _) = text_inline_text(settings, "line", height);
                // The image stands on its line's baseline.
                assert_eq!(end, baselines[1], "{settings:?} {height}");
                assert_eq!(end - top, height as i16);
                // Its line is only as tall as it must be above the baseline,
                // and keeps the bottom gap below.
                assert_eq!(
                    baselines[1] - baselines[0],
                    advance.max(image_top_gap(settings) + height as i16),
                    "{settings:?} {height}"
                );
                assert_eq!(
                    baselines[2] - baselines[1],
                    advance + image_bottom_gap(settings),
                    "{settings:?} {height}"
                );
                for style in STYLES {
                    let font = body_font(settings, style);
                    for (index, metric) in font.metrics.iter().enumerate() {
                        let codepoint = font.codepoints[index];
                        let first = i16::from(metric.y_offset);
                        let last = first + i16::from(metric.height) - 1;
                        assert!(
                            baselines[0] + last < top - 1,
                            "{settings:?} {style:?} U+{codepoint:04X} reaches row {} above an inline image at {top}",
                            baselines[0] + last
                        );
                        assert!(
                            baselines[2] + first > end,
                            "{settings:?} {style:?} U+{codepoint:04X} reaches row {} below an inline image ending at {end}",
                            baselines[2] + first
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn real_ink_clears_inline_images_on_the_page() {
        // The deepest and the tallest glyphs, in every style, on the lines
        // around a line holding an image.
        let mut text = String::new();
        for style in STYLES {
            text.push(STYLE_MARKER);
            text.push(style_marker_code(style));
            text.push_str("\u{1EB2}\u{1E4C}\u{C9}\u{C5}\u{122}gjpqy ");
        }
        let text = text.trim_end();
        for settings in every_body_type() {
            for height in [12, 40, 64] {
                let (_, (top, end), source) = text_inline_text(settings, text, height);
                let page = page_record_at(&source, 0);
                let mut fb = Framebuffer::new();
                let mut rect = None;
                draw_reading_page_body(&mut fb, &source, page, |_, image| {
                    rect = Some((image.x, image.width));
                });
                let (x, width) = rect.expect("an image");
                let inked = |y: i16| {
                    (x as usize..x as usize + width as usize)
                        .any(|column| !fb.pixel(column, y as usize))
                };
                // Nothing drawn in the image's rect or the rows that keep it
                // clear: the image draws there, and nothing else may.
                for y in top - 1..=end {
                    assert!(
                        !inked(y),
                        "{settings:?} {height}: ink at row {y}, image {top}..{end}"
                    );
                }
            }
        }
    }

    #[test]
    fn inline_images_stand_in_their_line_where_its_text_leaves_room() {
        let settings = TypeSettings::DEFAULT;
        let images = [(4, 17, 12), (5, 64, 64)];
        let line = inline_text("Complete the {0} AEBG and {1}HEY", &images);
        let source = inline_blocks(
            &[(line.clone(), TextAlign::Left, true, 64)],
            settings,
            PageBox::LANDSCAPE,
        );
        let page = page_record_at(&source, 0);
        let mut drawn = Vec::new();
        let mut fb = Framebuffer::new();
        draw_reading_page_body(&mut fb, &source, page, |_, image| {
            drawn.push((
                image.x,
                image.y,
                image.width,
                image.height,
                image.slot,
                String::from(image.alt),
            ));
        });
        let baseline = page_box_top() + inline_line_ascent(settings, source.blocks[0].0);
        let indent = paragraph_indent(settings.size);
        let left = PageBox::LANDSCAPE.left + indent;
        let before =
            |prefix: &str| left + styled_text_ink_width(prefix, settings, FontStyle::Regular);
        assert_eq!(
            drawn,
            [
                (
                    before("Complete the "),
                    baseline - 12,
                    17,
                    12,
                    Some(4),
                    String::new()
                ),
                (
                    before("Complete the ")
                        + 17
                        + styled_text_ink_width(" AEBG and ", settings, FontStyle::Regular),
                    baseline - 64,
                    64,
                    64,
                    Some(5),
                    String::new(),
                ),
            ]
        );
        // The line's width counts each image as its width.
        assert_eq!(
            inline_line_ink_width(&line, settings, FontStyle::Regular),
            styled_text_ink_width("Complete the  AEBG and HEY", settings, FontStyle::Regular)
                + 17
                + 64
        );
        // Placeholders outline each image in place and write nothing else.
        let mut fb = Framebuffer::new();
        draw_reading_page_body(&mut fb, &source, page, draw_image_placeholder);
        let (x, y, width, height, _, _) = drawn[0].clone();
        let (x, y) = (x as usize, y as usize);
        assert!(!fb.pixel(x, y) && !fb.pixel(x + width as usize - 1, y + height as usize - 1));
        assert!(fb.pixel(x + 3, y + 3), "an outline, empty inside");
    }

    fn page_box_top() -> i16 {
        PageBox::LANDSCAPE.top
    }

    #[test]
    fn justified_inline_lines_spread_their_gaps_around_whole_images() {
        let settings = TypeSettings::DEFAULT;
        let page_box = PageBox::LANDSCAPE;
        let words = "the parallelogram and the diagonal meet where the";
        let line = inline_text(
            &format!("Complete {{0}} {words} {{0}} ends"),
            &[(2, 17, 12)],
        );
        let natural = inline_line_ink_width(&line, settings, FontStyle::Regular);
        let target = page_box.right - READER_WRAP_SAFETY - page_box.left;
        assert!(
            natural < target && natural * 3 > target * 2,
            "{natural} of {target}"
        );
        let source = inline_blocks(
            &[
                (line.clone(), TextAlign::Justify, false, 12),
                ("last".into(), TextAlign::Justify, true, 0),
            ],
            settings,
            page_box,
        );
        // Not a paragraph opening: no indent.
        let mut source = source;
        source.starts = Some(vec![false, false]);
        let page = page_record_at(&source, 0);
        let mut images = Vec::new();
        let mut fb = Framebuffer::new();
        draw_reading_page_body(&mut fb, &source, page, |fb, image| {
            images.push(image.x);
            draw_image_placeholder(fb, image);
        });
        assert_eq!(images.len(), 2);
        // The second image moved right by the stretched gaps before it,
        // and the line's ink now reaches the margin.
        let natural_x = page_box.left
            + styled_text_ink_width(&format!("Complete  {words} "), settings, FontStyle::Regular)
            + 17;
        assert!(images[1] > natural_x, "{} > {natural_x}", images[1]);
        let baseline = page_box.top + inline_line_ascent(settings, source.blocks[0].0);
        let rightmost = (0..fb.frame_width())
            .rev()
            .find(|&x| (baseline - 30..baseline + 6).any(|y| !fb.pixel(x, y as usize)))
            .unwrap();
        assert!(
            (page_box.right - READER_WRAP_SAFETY - 3..page_box.right).contains(&(rightmost as i16)),
            "justified ink ends at {rightmost}"
        );
    }

    #[test]
    fn lines_holding_images_justify_whenever_two_thirds_full() {
        // A measure of 780 with nine four-pixel gaps: a text line takes 54
        // to spare at most (one and a half spaces a gap); a line holding
        // images takes anything up to a third of the measure.
        let (x, right) = (8, 792);
        let measure = right - READER_WRAP_SAFETY - x;
        assert_eq!(justify_extra(x, measure - 54, right, 9, 4), Some(54));
        assert_eq!(justify_extra(x, measure - 71, right, 9, 4), None);
        assert_eq!(justify_inline_extra(x, measure - 71, right, 9), Some(71));
        assert_eq!(justify_inline_extra(x, measure - 260, right, 4), Some(260));
        assert_eq!(justify_inline_extra(x, measure - 261, right, 4), None);
        assert_eq!(justify_inline_extra(x, measure - 10, right, 0), None);
        assert_eq!(justify_inline_extra(x, measure, right, 3), None);
    }

    #[test]
    fn pages_list_the_images_inside_their_lines() {
        let settings = TypeSettings::DEFAULT;
        let page_box = PageBox::LANDSCAPE;
        let mut source = inline_blocks(
            &[
                ("plain".into(), TextAlign::Left, true, 0),
                (
                    inline_text("a {0} b {1}", &[(3, 17, 12), (4, 30, 40)]),
                    TextAlign::Left,
                    true,
                    40,
                ),
                ("plain".into(), TextAlign::Left, true, 0),
            ],
            settings,
            page_box,
        );
        let page = |first_block, block_count| PageRecord {
            first_block,
            block_count,
        };
        assert!(!page_has_images(&source, page(0, 1)));
        assert!(page_has_images(&source, page(1, 1)));
        let list = |source: &ImageBlocks| {
            let mut seen = Vec::new();
            for_each_page_image(source, page(0, source.blocks.len() as u16), |slot, w, h| {
                seen.push((slot, w, h));
                true
            });
            seen
        };
        assert_eq!(list(&source), [(3, 17, 12), (4, 30, 40)]);
        // With an image block among them, in reading order; and a visit
        // that says stop, stops.
        let mut out = [0u8; proto::image::token::IMAGE_TOKEN_MAX_BYTES];
        let token =
            proto::image::token::encode_image_token(7, 300, Some("A plate"), &mut out).unwrap();
        source.blocks[0].0.image_height = 200;
        source.blocks[0].0.align = TextAlign::Center;
        source.blocks[0].1 = token.into();
        assert_eq!(list(&source), [(7, 300, 200), (3, 17, 12), (4, 30, 40)]);
        let mut first = None;
        for_each_page_image(&source, page(0, 3), |slot, _, _| {
            first.get_or_insert(slot);
            false
        });
        assert_eq!(first, Some(7));
    }

    #[test]
    fn damaged_inline_heights_stay_within_the_cap() {
        let settings = TypeSettings::DEFAULT;
        // A record whose height passes the cap, holding a token taller
        // than its record: the line is no taller than the cap allows, and
        // the image no taller than its line.
        let source = inline_blocks(
            &[
                (
                    inline_text("x {0}", &[(1, 900, 900)]),
                    TextAlign::Left,
                    true,
                    0x7FFF,
                ),
                (
                    inline_text("y {0}", &[(1, 20, 60)]),
                    TextAlign::Left,
                    true,
                    10,
                ),
            ],
            settings,
            PageBox::LANDSCAPE,
        );
        let most =
            image_top_gap(settings) + INLINE_IMAGE_MAX_HEIGHT as i16 + image_bottom_gap(settings);
        assert_eq!(block_height(&source, 0), most);
        let mut seen = Vec::new();
        for_each_page_image(&source, page_record_at(&source, 0), |_, w, h| {
            seen.push((w, h));
            true
        });
        let column = (PageBox::LANDSCAPE.right - PageBox::LANDSCAPE.left) as u16;
        assert_eq!(seen, [(column, INLINE_IMAGE_MAX_HEIGHT), (20, 10)]);
    }

    #[test]
    fn only_body_paragraph_openings_take_the_first_line_indent() {
        let blocks = IndentBlocks {
            //                0               1               2
            roles: &[
                TextRole::Body,
                TextRole::Body,
                TextRole::Body,
                TextRole::Heading1,
                TextRole::Body,
                TextRole::BlockQuote,
            ],
            aligns: &[
                TextAlign::Justify,
                TextAlign::Justify,
                TextAlign::Left,
                TextAlign::Center,
                TextAlign::Center,
                TextAlign::Left,
            ],
            // block 0 opens (index 0); block 1 continues it (0 did not end a
            // paragraph); block 2 opens (1 ended one); the rest each open.
            ends: &[false, true, true, true, true, true],
        };
        let indent = paragraph_indent(FontSize::Medium);
        assert_eq!(block_first_line_indent(&blocks, 0), indent, "opening line");
        assert_eq!(block_first_line_indent(&blocks, 1), 0, "continuation line");
        assert_eq!(block_first_line_indent(&blocks, 2), indent, "next opening");
        assert_eq!(block_first_line_indent(&blocks, 3), 0, "heading");
        assert_eq!(block_first_line_indent(&blocks, 4), 0, "centered body");
        assert_eq!(block_first_line_indent(&blocks, 5), 0, "blockquote");
    }

    #[test]
    fn first_line_indent_never_reduces_the_wrapped_line_count() {
        let font = body_font(TypeSettings::DEFAULT, FontStyle::Regular);
        let indent = paragraph_indent(FontSize::Medium);
        let max_width = READER_RIGHT_X - READER_LEFT_X;
        for sample in SAMPLES {
            let flush = wrapped_line_count(font, sample, max_width, 0);
            let indented = wrapped_line_count(font, sample, max_width, indent);
            assert!(
                indented >= flush,
                "indent must not drop lines: {sample:?} {flush} -> {indented}"
            );
        }
    }

    /// Reference implementation: measure the whole string from scratch,
    /// exactly as the pre-incremental firmware code did.
    fn naive_text_ink_width(font: &'static BitmapFont, text: &str) -> i16 {
        let mut advance_fp = 0i32;
        let mut right = 0i16;
        let mut previous = None;
        for ch in text.chars() {
            let codepoint = if ch as u32 > u16::MAX as u32 {
                b'?' as u16
            } else {
                ch as u16
            };
            let (drawn, metric) = if let Some((metric, _)) = font.glyph(codepoint) {
                (codepoint, metric)
            } else if let Some((metric, _)) = font.glyph(b'?' as u16) {
                (b'?' as u16, metric)
            } else {
                advance_fp += 8 << 4;
                right = right.max(fixed_ceil(advance_fp));
                continue;
            };
            if let Some(left) = previous {
                advance_fp += font.kerning_adjust_fp(left, drawn) as i32;
            }
            let advance = fixed_round(advance_fp);
            let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
            right = right.max(glyph_right);
            advance_fp += metric.advance_fp as i32;
            previous = Some(drawn);
        }
        right.max(fixed_ceil(advance_fp))
    }

    /// Reference wrap: re-measures every candidate line per word, exactly
    /// as the pre-incremental firmware code did.
    fn naive_next_wrapped_line(
        text: &str,
        mut cursor: usize,
        font: &'static BitmapFont,
        x: i16,
        max_x: i16,
    ) -> Option<(usize, usize, usize)> {
        let bytes = text.as_bytes();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }

        let mut scan = cursor;
        let mut best_end = cursor;
        let mut best_next = cursor;
        while scan < bytes.len() {
            let word_start = scan;
            while scan < bytes.len() && !bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            let word_end = scan;
            let candidate = &text[cursor..word_end];
            if x + naive_text_ink_width(font, candidate) + READER_WRAP_SAFETY > max_x {
                if best_end == cursor {
                    return Some((word_start, word_end, word_end));
                }
                return Some((cursor, best_end, best_next));
            }
            best_end = word_end;
            while scan < bytes.len() && bytes[scan].is_ascii_whitespace() {
                scan += 1;
            }
            best_next = scan;
        }

        Some((cursor, best_end, best_next.max(best_end)))
    }

    const SAMPLES: &[&str] = &[
        "",
        " ",
        "word",
        "two words",
        "It was the best of times, it was the worst of times, it was the age of wisdom.",
        "  leading and   irregular   spacing between words  ",
        "Supercalifragilisticexpialidocious-antidisestablishmentarianism-longword",
        "short a b c d e f g h i j k l m n o p q r s t u v w x y z",
        "punctuation, everywhere! (parentheses) \"quotes\" -- dashes; colons: done.",
        "tabs\tand\nnewlines\ras whitespace",
        "non-latin \u{4e16}\u{754c} mixed with latin text and \u{20ac} symbols",
        "beyond bmp \u{1F600} falls back to question mark",
    ];

    const STYLES: [FontStyle; 4] = [
        FontStyle::Regular,
        FontStyle::Italic,
        FontStyle::Bold,
        FontStyle::BoldItalic,
    ];

    const ALL_SETTINGS: [TypeSettings; 54] = {
        let sizes = [FontSize::Small, FontSize::Medium, FontSize::Large];
        let spacings = [
            LineSpacing::Compact,
            LineSpacing::Normal,
            LineSpacing::Relaxed,
        ];
        let weights = [FontWeight::Normal, FontWeight::Heavy];
        let families = [
            FontFamily::Literata,
            FontFamily::Merriweather,
            FontFamily::Custom,
        ];
        let mut out = [TypeSettings::DEFAULT; 54];
        let mut i = 0;
        while i < 3 {
            let mut j = 0;
            while j < 3 {
                let mut k = 0;
                while k < 2 {
                    let mut l = 0;
                    while l < 3 {
                        out[((i * 3 + j) * 2 + k) * 3 + l] = TypeSettings {
                            size: sizes[i],
                            spacing: spacings[j],
                            weight: weights[k],
                            family: families[l],
                            alignment: Alignment::Justified,
                        };
                        l += 1;
                    }
                    k += 1;
                }
                j += 1;
            }
            i += 1;
        }
        out
    };

    fn fonts() -> [&'static BitmapFont; 4] {
        STYLES.map(|style| body_font(TypeSettings::DEFAULT, style))
    }

    #[test]
    fn ink_width_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                assert_eq!(
                    text_ink_width(font, sample),
                    naive_text_ink_width(font, sample),
                    "sample {sample:?}"
                );
            }
        }
    }

    #[test]
    fn next_wrapped_line_matches_naive_reference() {
        for font in fonts() {
            for sample in SAMPLES {
                for max_x in [40i16, 120, 300, 784] {
                    for x in [0i16, 8, 32] {
                        let mut cursor = 0usize;
                        loop {
                            let fast = next_wrapped_line(sample, cursor, font, x, max_x);
                            let slow = naive_next_wrapped_line(sample, cursor, font, x, max_x);
                            assert_eq!(fast, slow, "sample {sample:?} x {x} max_x {max_x}");
                            let Some((_, _, next_cursor)) = fast else {
                                break;
                            };
                            assert!(next_cursor > cursor, "wrap must make progress");
                            cursor = next_cursor;
                            if cursor >= sample.len() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn styled_width_matches_unstyled_when_unmarked() {
        for settings in ALL_SETTINGS {
            for style in STYLES {
                for sample in SAMPLES {
                    assert_eq!(
                        styled_text_ink_width(sample, settings, style),
                        naive_text_ink_width(body_font(settings, style), sample),
                        "sample {sample:?} settings {settings:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn layout_configs_are_distinct_per_type_settings_and_page_box() {
        let mut seen = [0u16; 4 * ALL_SETTINGS.len()];
        let mut index = 0;
        for portrait in [false, true] {
            for alignment in [Alignment::Justified, Alignment::Left] {
                for settings in ALL_SETTINGS.iter() {
                    let settings = TypeSettings {
                        alignment,
                        ..*settings
                    };
                    let config = reader_layout_config(settings, portrait);
                    assert!(
                        !seen[..index].contains(&config),
                        "duplicate layout config {config} for {settings:?} portrait={portrait}"
                    );
                    seen[index] = config;
                    index += 1;
                }
            }
        }
    }

    #[test]
    fn justified_layout_configs_predate_the_alignment_setting() {
        // Caches built before the setting existed carry these configs; a
        // justified reader must keep reading them without a rebuild.
        let config = reader_layout_config(TypeSettings::DEFAULT, false);
        assert_eq!(config & 0x8000, 0);
        let left = TypeSettings {
            alignment: Alignment::Left,
            ..TypeSettings::DEFAULT
        };
        assert_eq!(reader_layout_config(left, false), config | 0x8000);
    }

    #[test]
    fn left_alignment_only_overrides_justified_text() {
        let left = TypeSettings {
            alignment: Alignment::Left,
            ..TypeSettings::DEFAULT
        };
        assert_eq!(effective_align(TextAlign::Justify, left), TextAlign::Left);
        assert_eq!(effective_align(TextAlign::Center, left), TextAlign::Center);
        assert_eq!(effective_align(TextAlign::Left, left), TextAlign::Left);
        assert_eq!(
            effective_align(TextAlign::Justify, TypeSettings::DEFAULT),
            TextAlign::Justify
        );
    }

    #[test]
    fn line_advances_grow_with_size_and_spacing() {
        let settings = [
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Small,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Medium,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Compact,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Normal,
                ..TypeSettings::DEFAULT
            },
            TypeSettings {
                size: FontSize::Large,
                spacing: LineSpacing::Relaxed,
                ..TypeSettings::DEFAULT
            },
        ];
        for role in [TextRole::Body, TextRole::Heading1] {
            for window in settings.windows(2) {
                assert!(
                    line_advance(window[0], role) < line_advance(window[1], role)
                        || window[0].size != window[1].size,
                    "advance must grow with spacing within a size: {window:?}"
                );
            }
        }
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Body), 26);
        assert_eq!(line_advance(TypeSettings::DEFAULT, TextRole::Heading1), 31);
    }

    #[test]
    fn styled_cursor_checkpoint_equals_full_measure() {
        let mut styled = heapless::String::<256>::new();
        let _ = styled.push_str("plain ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Italic));
        let _ = styled.push_str("slanted words ");
        let _ = styled.push(STYLE_MARKER);
        let _ = styled.push(style_marker_code(FontStyle::Bold));
        let _ = styled.push_str("heavy end");

        // Push in arbitrary fragments; the running width must match a
        // one-shot measure at every fragment boundary, at every size.
        for settings in ALL_SETTINGS {
            let text = styled.as_str();
            let mut cursor = StyledInkCursor::new(settings, FontStyle::Regular);
            let mut consumed = 0usize;
            for chunk in [3usize, 1, 9, 2, 30, 200] {
                let end = (consumed + chunk).min(text.len());
                while !text.is_char_boundary(consumed.min(text.len())) {
                    consumed += 1;
                }
                let mut safe_end = end;
                while safe_end < text.len() && !text.is_char_boundary(safe_end) {
                    safe_end += 1;
                }
                cursor.push_str(&text[consumed..safe_end]);
                consumed = safe_end;
                assert_eq!(
                    cursor.width(),
                    styled_text_ink_width(&text[..consumed], settings, FontStyle::Regular),
                    "prefix {:?} settings {settings:?}",
                    &text[..consumed]
                );
                if consumed >= text.len() {
                    break;
                }
            }
        }
    }

    /// One stored reading line, given as its true style runs. A run's
    /// spaces end it, as the line builder writes them (the space before a
    /// style change goes ahead of the marker).
    struct StyleCase {
        name: &'static str,
        runs: &'static [(FontStyle, &'static str)],
        align: TextAlign,
    }

    const STYLE_CASES: &[StyleCase] = &[
        StyleCase {
            name: "plain lead-in, then italic",
            runs: &[
                (FontStyle::Regular, "At last the Mouse, who seemed to be a person of "),
                (FontStyle::Italic, "authority "),
                (FontStyle::Regular, "among them,"),
            ],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "plain lead-in, then an italic quotation",
            runs: &[
                (FontStyle::Regular, "called out \u{201C}"),
                (FontStyle::Italic, "Sit down, all of you, and listen to me!"),
                (FontStyle::Regular, "\u{201D} I\u{2019}ll soon make you"),
            ],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "italic run continued from the line before",
            runs: &[
                (FontStyle::Italic, "dry enough!"),
                (FontStyle::Regular, "\u{201D} They all sat down at once, in a large ring, with the Mouse"),
            ],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "plain lead-in, then bold",
            runs: &[
                (FontStyle::Regular, "the Mouse in the middle. Alice kept her eyes "),
                (FontStyle::Bold, "anxiously fixed "),
                (FontStyle::Regular, "on it,"),
            ],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "plain lead-in, then bold italic",
            runs: &[
                (FontStyle::Regular, "sure she would catch a "),
                (FontStyle::BoldItalic, "bad cold "),
                (FontStyle::Regular, "if she did not get dry very soon, and"),
            ],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "centred, plain lead-in, then italic",
            runs: &[
                (FontStyle::Regular, "The "),
                (FontStyle::Italic, "Mouse\u{2019}s "),
                (FontStyle::Regular, "Long and Sad Tale"),
            ],
            align: TextAlign::Center,
        },
        StyleCase {
            name: "only emphasis",
            runs: &[(
                FontStyle::Italic,
                "\u{201C}Ahem!\u{201D} said the Mouse with an important air, \u{201C}are you all ready? This",
            )],
            align: TextAlign::Justify,
        },
        StyleCase {
            name: "no emphasis",
            runs: &[(
                FontStyle::Regular,
                "This is the driest thing I know. Silence all round, if you please! William",
            )],
            align: TextAlign::Justify,
        },
    ];

    fn push_marker(out: &mut String, style: FontStyle) {
        out.push(STYLE_MARKER);
        out.push(style_marker_code(style));
    }

    /// The line as the builder stores it: it opens in Regular and marks a
    /// run only where the style changes.
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

    /// The same line with every run marked, so no opening style can
    /// change how it draws.
    fn marked_text(runs: &[(FontStyle, &str)]) -> String {
        let mut out = String::new();
        for (style, text) in runs {
            push_marker(&mut out, *style);
            out.push_str(text);
        }
        out
    }

    /// The block style v0.6.1's builder stored: the line's first marker,
    /// wherever it sits.
    fn v061_block_style(text: &str) -> FontStyle {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == STYLE_MARKER {
                return chars
                    .next()
                    .and_then(style_from_marker_code)
                    .unwrap_or(FontStyle::Regular);
            }
        }
        FontStyle::Regular
    }

    /// A page of one stored line.
    struct OneLine<'a> {
        text: &'a str,
        style: FontStyle,
        align: TextAlign,
        paragraph_end: bool,
        settings: TypeSettings,
    }

    impl ReadingBlocks for OneLine<'_> {
        fn block_count(&self) -> usize {
            1
        }
        fn block(&self, index: usize) -> Option<BlockRecord> {
            (index == 0).then_some(BlockRecord {
                text_offset: 0,
                text_len: self.text.len() as u16,
                line_count: 1,
                role: TextRole::Body,
                style: proto::text::FontStyle::Regular,
                align: self.align,
                image_height: 0,
            })
        }
        fn block_text(&self, _index: usize) -> &str {
            self.text
        }
        fn block_style(&self, _index: usize) -> FontStyle {
            self.style
        }
        fn page_break_before(&self, _index: usize) -> bool {
            false
        }
        fn paragraph_end(&self, _index: usize) -> bool {
            self.paragraph_end
        }
        fn type_settings(&self) -> TypeSettings {
            self.settings
        }
    }

    const ONE_LINE_PAGE: PageRecord = PageRecord {
        first_block: 0,
        block_count: 1,
    };

    fn draw_one_line(source: &OneLine<'_>) -> Box<Framebuffer> {
        let mut fb = Box::new(Framebuffer::new());
        draw_reading_page_body(&mut fb, source, ONE_LINE_PAGE, |_, _| {});
        fb
    }

    /// The style each visible word starts in, read off the runs.
    fn expected_word_styles(runs: &[(FontStyle, &str)]) -> Vec<(String, FontStyle)> {
        let mut words: Vec<(String, FontStyle)> = Vec::new();
        let mut in_word = false;
        for (style, text) in runs {
            for ch in text.chars() {
                if ch == ' ' {
                    in_word = false;
                } else if in_word {
                    words.last_mut().unwrap().0.push(ch);
                } else {
                    words.push((ch.into(), *style));
                    in_word = true;
                }
            }
        }
        words
    }

    /// The style each visible word starts in, as the justified drawers
    /// (built-in and custom-font) walk a stored line from `opening`.
    fn walked_word_styles(text: &str, opening: FontStyle) -> Vec<(String, FontStyle)> {
        let mut words = Vec::new();
        walk_styled_words(text, opening, |step| {
            if let StyledWordStep::Word(segment, mut style) = step {
                let mut visible = String::new();
                let mut chars = segment.chars();
                while let Some(ch) = chars.next() {
                    if ch == STYLE_MARKER {
                        let code = chars.next().and_then(style_from_marker_code);
                        if visible.is_empty() {
                            style = code.unwrap_or(style);
                        }
                    } else {
                        visible.push(ch);
                    }
                }
                words.push((visible, style));
            }
            true
        });
        words
    }

    /// The page drawn run by run, each in its own face: the line as it
    /// should look at natural spacing.
    fn reference_natural_line(
        runs: &[(FontStyle, &str)],
        source: &OneLine<'_>,
    ) -> Box<Framebuffer> {
        let settings = source.settings;
        let page_box = source.page_box();
        let marked = marked_text(runs);
        let mut x = match effective_align(source.align, settings) {
            TextAlign::Center => {
                let width = styled_text_ink_width(&marked, settings, FontStyle::Regular)
                    .min(page_box.right - page_box.left);
                ((page_box.left + page_box.right - width) / 2).max(page_box.left)
            }
            TextAlign::Left | TextAlign::Justify => {
                page_box.x_for(TextRole::Body) + block_first_line_indent(source, 0)
            }
        };
        let y = page_box.top + line_advance(settings, TextRole::Body);
        let mut fb = Box::new(Framebuffer::new());
        for (style, text) in runs {
            x = draw_text(&mut fb, body_font(settings, *style), text, x, y, false);
        }
        fb
    }

    const STYLE_CASE_SETTINGS: [TypeSettings; 6] = [
        TypeSettings::DEFAULT,
        TypeSettings {
            family: FontFamily::Merriweather,
            ..TypeSettings::DEFAULT
        },
        TypeSettings {
            weight: FontWeight::Heavy,
            ..TypeSettings::DEFAULT
        },
        TypeSettings {
            size: FontSize::Small,
            spacing: LineSpacing::Compact,
            ..TypeSettings::DEFAULT
        },
        TypeSettings {
            size: FontSize::Large,
            ..TypeSettings::DEFAULT
        },
        TypeSettings {
            alignment: Alignment::Left,
            ..TypeSettings::DEFAULT
        },
    ];

    #[test]
    fn a_line_opens_in_regular_unless_a_marker_leads_it() {
        for case in STYLE_CASES {
            let stored = builder_text(case.runs);
            let opening = case.runs[0].0;
            for block_style in [v061_block_style(&stored), FontStyle::Regular] {
                assert_eq!(
                    line_opening_style(&stored, block_style),
                    opening,
                    "{}: stored {stored:?} as {block_style:?}",
                    case.name
                );
            }
            assert_eq!(
                line_opening_style(&marked_text(case.runs), FontStyle::Bold),
                opening,
                "{}: a leading marker decides",
                case.name
            );
        }
        // Text without markers draws whole in its block style (host
        // fixtures' italic block quotes, wrapped over several lines).
        for style in STYLES {
            assert_eq!(line_opening_style("a quoted aside", style), style);
        }
        assert_eq!(line_opening_style("", FontStyle::Italic), FontStyle::Italic);
    }

    /// Every case, as caches before the fix stored it (block style = first
    /// marker) and as the builder stores it now, draws each run in its own
    /// face: the same pixels as the line with every run marked, and at
    /// natural spacing the same pixels as drawing the runs one by one.
    #[test]
    fn stored_lines_draw_and_measure_every_run_in_its_own_style() {
        for settings in STYLE_CASE_SETTINGS {
            for case in STYLE_CASES {
                let stored = builder_text(case.runs);
                let marked = marked_text(case.runs);
                let opening = case.runs[0].0;
                let old_style = v061_block_style(&stored);
                let new_style = line_opening_style(&stored, FontStyle::Regular);
                for paragraph_end in [false, true] {
                    let line = |text, style| OneLine {
                        text,
                        style,
                        align: case.align,
                        paragraph_end,
                        settings,
                    };
                    let label = format!(
                        "{} (paragraph end {paragraph_end}, {settings:?})",
                        case.name
                    );
                    // The style the drawers start from, the built-in and
                    // custom-font page bodies alike.
                    for style in [old_style, new_style] {
                        let mut seen = None;
                        for_each_drawable_block(&line(&stored, style), ONE_LINE_PAGE, |block| {
                            seen = Some(block.style);
                            true
                        });
                        assert_eq!(seen, Some(opening), "{label}: drawable style");
                    }
                    assert_eq!(
                        walked_word_styles(&stored, opening),
                        expected_word_styles(case.runs),
                        "{label}: justified word styles"
                    );

                    let want = draw_one_line(&line(&marked, opening));
                    for style in [old_style, new_style] {
                        assert!(
                            draw_one_line(&line(&stored, style)).bytes() == want.bytes(),
                            "{label}: stored as {style:?} draws unlike the marked line"
                        );
                    }
                    let natural = paragraph_end
                        || effective_align(case.align, settings) != TextAlign::Justify;
                    if natural {
                        assert!(
                            reference_natural_line(case.runs, &line(&stored, old_style)).bytes()
                                == want.bytes(),
                            "{label}: draws unlike its runs drawn one by one"
                        );
                    }
                }
            }
        }
    }

    /// The cases above would catch the bug: drawing from the stored first
    /// marker (the old start style) paints a plain lead-in in emphasis, and
    /// the long body lines really are justified mid-paragraph.
    #[test]
    fn the_old_start_style_draws_a_plain_lead_in_wrongly() {
        let settings = TypeSettings::DEFAULT;
        let mut wrong = 0;
        for case in STYLE_CASES {
            let stored = builder_text(case.runs);
            let old_style = v061_block_style(&stored);
            for paragraph_end in [false, true] {
                let line = |text, style| OneLine {
                    text,
                    style,
                    align: case.align,
                    paragraph_end,
                    settings,
                };
                let mut old_start = String::new();
                push_marker(&mut old_start, old_style);
                old_start.push_str(&stored);
                let fixed = draw_one_line(&line(&stored, old_style));
                let old = draw_one_line(&line(&old_start, old_style));
                let differs = fixed.bytes() != old.bytes();
                assert_eq!(
                    differs,
                    old_style != case.runs[0].0,
                    "{}: paragraph end {paragraph_end}",
                    case.name
                );
                wrong += usize::from(differs);
                if case.align == TextAlign::Justify && !paragraph_end {
                    let natural = draw_one_line(&OneLine {
                        paragraph_end: true,
                        ..line(&stored, old_style)
                    });
                    assert!(
                        natural.bytes() != fixed.bytes(),
                        "{}: a mid-paragraph line should stretch",
                        case.name
                    );
                }
            }
        }
        assert_eq!(wrong, 10, "five cases open plain before emphasis");
    }
}
