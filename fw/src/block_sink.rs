//! The XHTML block sink a spine's layout streams into: it normalizes the
//! parser's runs of text, builds pre-wrapped lines (hyphenating justified
//! prose), lays images out from the book's index, pushes everything into
//! the store's section arena, and writes a section file whenever the arena
//! fills. `reader_cache::lay_out_spine` builds one per layout pass.
//!
//! Kept apart from the rest of the reader cache, which drives the card and
//! the panel, so host tests can run it against an in-memory card.

use crate::book_images::{BookImages, ImagePlacement};
use crate::line_window::{End, LineWindow, Measure, Span};
use crate::reader_cache_files;
use crate::reader_layout;
use crate::reader_store::{ReaderStore, MAX_READER_BLOCK_TEXT, MAX_READER_PAGES};
use display::font::{fixed_ceil, fixed_round, FontFamily, FontStyle, TypeSettings};
use embassy_time::Instant;
use embedded_sdmmc::{Directory, TimeSource};
use heapless::String;
use proto::cache::BookV2SectionRecord;
use proto::epub::{
    alt_is_initial, image_alt_text, push_image_placeholder, XhtmlBlockSink, XhtmlError,
    MAX_ENTRY_NAME_BYTES,
};
use proto::hyphen::{word_breaks, Hyphenation, WordBreak, MAX_WORD_BREAKS};
use proto::image::index::ImagePlace;
use proto::image::token::{
    decode_image_token, encode_image_token, encode_inline_image_token, inline_pieces,
    InlineImageToken, InlinePiece, IMAGE_TOKEN_MARK, IMAGE_TOKEN_MAX_BYTES,
    INLINE_IMAGE_TOKEN_BYTES,
};
use proto::text::{TextAlign, TextRole};
use ui::reading::{InkPiece, StyledInkCursor};

/// What a call to `build_or_load_book_cache` is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Preparation {
    /// A reader request: answer it as soon as the prepared prefix holds
    /// its page, preparing one more spine per call until it does.
    Request,
    /// Idle-time growth of the open book: prepare its next spine, keep the
    /// requested page (the one on screen) resident, and give way the moment
    /// a button press lands after `presses` was read from
    /// `crate::BUTTON_PRESSES`.
    Idle { presses: u32 },
}

impl Preparation {
    pub(crate) fn gives_way(self) -> bool {
        match self {
            Self::Request => false,
            Self::Idle { presses } => {
                crate::BUTTON_PRESSES.load(core::sync::atomic::Ordering::Relaxed) != presses
            }
        }
    }
}

/// Which section table a spine's sections join.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpineRun {
    Prefix,
    Seed,
}

pub(crate) struct LibraryBlockSink<
    'a,
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    pub(crate) library: &'a mut ReaderStore,
    pub(crate) root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    /// The book's SECTIONS directory, opened once for the whole build.
    /// `None` when the cache directories could not be created — section
    /// writes then fail per section and the book publishes as partial.
    pub(crate) sections_dir: Option<&'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    pub(crate) source_identity: (u32, u32),
    /// Id the run's first section takes.
    pub(crate) section_base: u16,
    /// The table this spine's sections join.
    pub(crate) run: SpineRun,
    /// The run's table filled before the spine was through.
    pub(crate) run_full: bool,
    pub(crate) font_metrics: &'a mut crate::custom_font::MetricCache,
    pub(crate) book_partial: &'a mut bool,
    /// Accumulated wall time spent writing section files, for the
    /// `bench: storage_build` line.
    pub(crate) write_micros: &'a mut u64,
    pub(crate) spine_index: u16,
    pub(crate) line: String<MAX_READER_BLOCK_TEXT>,
    /// Running ink width of `line`, measured from Regular: a line opens in
    /// Regular unless a style marker leads it (`line_opening_style`), so the
    /// running width matches how the line draws.
    pub(crate) line_ink: LineInkCursor,
    pub(crate) line_role: TextRole,
    pub(crate) line_align: TextAlign,
    pub(crate) line_style: FontStyle,
    pub(crate) pending_space: bool,
    pub(crate) dropping_paragraph: bool,
    /// Patterns for the book's language; soft hyphens and dashes break in
    /// any language.
    pub(crate) hyphenation: Hyphenation,
    /// Lines in a row that ended in a hyphen, and whether the line in
    /// progress will.
    pub(crate) hyphen_run: u8,
    pub(crate) line_hyphenated: bool,
    pub(crate) stopped: bool,
    /// An idle-time step stops at the next block once a button is pressed,
    /// and says so here: unlike `stopped`, nothing it built is kept.
    pub(crate) preparation: Preparation,
    pub(crate) interrupted: bool,
    pub(crate) target_pages: usize,
    pub(crate) generate_toc_from_headings: bool,
    pub(crate) generated_toc_for_spine: bool,
    /// Only appends and final paragraph-end marks occur between updates.
    /// Completed pages stay valid; the dirty trailing page is repaginated.
    pub(crate) pages_dirty: bool,
    pub(crate) write_failed: bool,
    /// The book's image index, when it has images.
    pub(crate) images: Option<&'a BookImages<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    /// The chapter's ZIP path, which its images' sources are relative to.
    pub(crate) chapter: &'a str,
    /// Where an image source resolves to: the ZIP name scratch, idle while
    /// the chapter streams.
    pub(crate) image_path: &'a mut [u8; MAX_ENTRY_NAME_BYTES],
    /// The spine's first layout pass, in which an unprobed image waits for
    /// a probe; in the second it shows as alt text.
    pub(crate) first_pass: bool,
    /// The first and last index slots of the unprobed images this pass
    /// met. Once set, the pass is only looking for more: the spine is laid
    /// out again after they are probed.
    pub(crate) unprobed: Option<(u16, u16)>,
    /// Images this pass has looked up so far.
    pub(crate) images_met: u16,
    /// Images met at the paragraph's start, before any of its text, until
    /// the paragraph shows whether text follows them.
    pub(crate) pending: PendingImages<'a>,
    /// The paragraph in hand holds text, not just whitespace: an image met
    /// now goes inside its line.
    pub(crate) paragraph_has_text: bool,
    /// The tallest image inside the line in hand; 0 for none.
    pub(crate) line_image_height: u16,
    /// A drop cap opens the line in hand: it takes no first-line indent,
    /// and its record says it continues a paragraph so the page draws it
    /// so (Project Gutenberg sets such a paragraph `unindent`).
    pub(crate) line_unindented: bool,
    /// The words of the last few justified lines, which a line left ragged
    /// or loose takes back to break again.
    pub(crate) window: LineWindow<'a>,
}

/// What the book's index says of an image a chapter names.
enum Lookup {
    /// An image the decoder takes: its sizes alone in a paragraph and
    /// inside a line (`None` for an illustration too tall for a line), and
    /// whether its entry already says it is shown inline.
    Shown {
        slot: u16,
        block: (u16, u16),
        inline: Option<(u16, u16)>,
        inline_marked: bool,
    },
    /// Nothing to show: its alt text stands in.
    Missing,
    /// Nothing to lay out in this pass: an unprobed image, or one met after
    /// it in a pass that is laid out again.
    Skip,
    /// The spine stops here.
    Stop,
}

/// What a run of text in a book with images does before it is placed
/// ([`LibraryBlockSink::ahead_of_text`]).
enum Ahead<'t> {
    /// It is placed as it comes.
    Text(&'t str),
    /// The images held go inside the line first, at this alignment, then
    /// the run (a drop cap's word glued to it).
    PlaceHeld(&'t str, TextAlign),
    /// The images held go out as blocks first: a drop cap that a heading,
    /// a centred line or a scene break follows, or a run of spacing there
    /// is no room to hold.
    CommitHeld(&'t str),
    /// Only spacing, now held in order with the images.
    Spacing,
}

/// Where [`LibraryBlockSink::hold_image`] put an image.
enum Hold {
    /// Held back until its paragraph shows whether text follows.
    Pending,
    /// Held to go inside the line at once: its paragraph has text.
    Inline,
    /// No room to hold it: the block it would be, or its alt-text
    /// placeholder.
    Block(ShownImage),
    Placeholder,
}

/// What a held record is (see [`PendingImages`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// An image the decoder takes; its record's text is its block token.
    Shown(ShownImage),
    /// An image that cannot be shown; its record's text is the alt text
    /// that stands in for it.
    Missing,
    /// A run of spacing met among the images (spaces, no-break spaces),
    /// kept so it is laid out in its place either way; its record's text
    /// is the run, pushed with this role, style and alignment.
    Spacing {
        role: TextRole,
        style: FontStyle,
        align: TextAlign,
    },
}

/// A record held back at a paragraph's start (see [`PendingImages`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingImage {
    pub(crate) held: Held,
    /// The source had a space before it.
    pub(crate) leading_space: bool,
    /// A drop cap: alone in a block that floats left, with a single letter
    /// for its alt. It may join the paragraph after its own.
    pub(crate) drop_cap: bool,
}

/// An image the decoder takes, as held back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShownImage {
    pub(crate) slot: u16,
    pub(crate) block: (u16, u16),
    pub(crate) inline: (u16, u16),
    pub(crate) inline_marked: bool,
}

/// Bytes of a held record before its text.
const PENDING_HEAD_BYTES: usize = 14;
/// Most records held back at once; past it (or past the scratch), the
/// images held go out as the blocks they would be if no text followed.
const MAX_PENDING_IMAGES: u8 = 32;
/// Longest alt text a held image keeps: the placeholder's bound.
const HELD_ALT_BYTES: usize = 160;

/// Images met at a paragraph's start, before any of its text, and the
/// spacing among them. The parser streams, so whether such an image goes
/// inside a line (text follows in its paragraph) or stands alone as a block
/// (the paragraph ends first) is known only later. Each is held as a record
/// in borrowed scratch, the idle tail of the container scratch, so nothing
/// is copied onto the parse callbacks' stack: a [`PENDING_HEAD_BYTES`]
/// head, then the text it needs, its image token, the alt text that stands
/// in for it, or the run of spacing.
#[derive(Default)]
pub(crate) struct PendingImages<'a> {
    buf: &'a mut [u8],
    len: usize,
    count: u8,
    /// Records held since the last paragraph end.
    fresh: u8,
    /// The records were carried over from a floated block ahead of the
    /// paragraph in hand.
    carried: bool,
}

impl<'a> PendingImages<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            ..Self::default()
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Holds `record`, with the text `write` puts in the space it is
    /// handed (returning its length, or `None` when it does not fit
    /// whole). False, holding nothing, when there is no room for both.
    fn push(
        &mut self,
        record: PendingImage,
        write: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> bool {
        if self.count >= MAX_PENDING_IMAGES {
            return false;
        }
        let Some(room) = self.buf.get_mut(self.len..) else {
            return false;
        };
        let Some((head, text)) = room.split_at_mut_checked(PENDING_HEAD_BYTES) else {
            return false;
        };
        let Some(text_len) = write(text).and_then(|len| u16::try_from(len).ok()) else {
            return false;
        };
        let mut fields = [0u16; 5];
        head[0] = match record.held {
            Held::Missing => 0,
            Held::Shown(shown) => {
                fields = [
                    shown.slot,
                    shown.block.0,
                    shown.block.1,
                    shown.inline.0,
                    shown.inline.1,
                ];
                1 | u8::from(shown.inline_marked) << 4
            }
            Held::Spacing { role, style, align } => {
                fields[0] = u16::from(role_code(role))
                    | u16::from(display::font::style_marker_code(style) as u8) << 8;
                fields[1] = u16::from(align_code(align));
                2
            }
        };
        head[1] = u8::from(record.leading_space) | u8::from(record.drop_cap) << 1;
        for (index, value) in fields.into_iter().chain([text_len]).enumerate() {
            head[2 + 2 * index..4 + 2 * index].copy_from_slice(&value.to_le_bytes());
        }
        self.len += PENDING_HEAD_BYTES + usize::from(text_len);
        self.count += 1;
        self.fresh += 1;
        true
    }

    /// The held records in order, each with its text.
    fn iter(&self) -> impl Iterator<Item = (PendingImage, &str)> + '_ {
        let mut at = 0usize;
        (0..self.count).map_while(move |_| {
            let head = self.buf.get(at..at + PENDING_HEAD_BYTES)?;
            let field =
                |index: usize| u16::from_le_bytes([head[2 + 2 * index], head[3 + 2 * index]]);
            let text_at = at + PENDING_HEAD_BYTES;
            let text = self.buf.get(text_at..text_at + usize::from(field(5)))?;
            at = text_at + text.len();
            let held = match head[0] & 0xf {
                1 => Held::Shown(ShownImage {
                    slot: field(0),
                    block: (field(1), field(2)),
                    inline: (field(3), field(4)),
                    inline_marked: head[0] & 0x10 != 0,
                }),
                2 => Held::Spacing {
                    role: role_from_code(field(0) as u8),
                    style: display::font::style_from_marker_code(char::from((field(0) >> 8) as u8))
                        .unwrap_or(FontStyle::Regular),
                    align: align_from_code(field(1) as u8),
                },
                _ => Held::Missing,
            };
            let record = PendingImage {
                held,
                leading_space: head[1] & 1 != 0,
                drop_cap: head[1] & 2 != 0,
            };
            Some((record, core::str::from_utf8(text).unwrap_or("")))
        })
    }

    fn clear(&mut self) {
        self.len = 0;
        self.count = 0;
        self.fresh = 0;
        self.carried = false;
    }

    /// A lone drop cap held in the paragraph just ended, which carries
    /// into the next.
    fn holds_lone_drop_cap(&self) -> bool {
        self.count == 1
            && self.fresh == 1
            && !self.carried
            && self.iter().all(|(record, _)| record.drop_cap)
    }
}

fn role_code(role: TextRole) -> u8 {
    match role {
        TextRole::Body => 0,
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::BlockQuote => 4,
    }
}

fn role_from_code(code: u8) -> TextRole {
    match code {
        1 => TextRole::Heading1,
        2 => TextRole::Heading2,
        3 => TextRole::Heading3,
        4 => TextRole::BlockQuote,
        _ => TextRole::Body,
    }
}

fn align_code(align: TextAlign) -> u8 {
    match align {
        TextAlign::Left => 0,
        TextAlign::Center => 1,
        TextAlign::Justify => 2,
    }
}

fn align_from_code(code: u8) -> TextAlign {
    match code {
        0 => TextAlign::Left,
        1 => TextAlign::Center,
        _ => TextAlign::Justify,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum LineInkCursor {
    BuiltIn(StyledInkCursor),
    Custom(CustomLineInkCursor),
}

impl LineInkCursor {
    pub(crate) fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        if settings.family == FontFamily::Custom && !display::font::builtin_custom_available() {
            Self::Custom(CustomLineInkCursor::new(settings, default_style))
        } else {
            Self::BuiltIn(StyledInkCursor::new(settings, default_style))
        }
    }

    fn width(&self) -> i16 {
        match self {
            Self::BuiltIn(cursor) => cursor.width(),
            Self::Custom(cursor) => cursor.width(),
        }
    }

    /// An inline image `width` columns wide.
    fn push_object(&mut self, width: u16) {
        match self {
            Self::BuiltIn(cursor) => cursor.push_object(width),
            Self::Custom(cursor) => cursor.push_object(width),
        }
    }

    /// A word in `style` measured on its own, after what joins it to the
    /// glyph before: as pushing their text would measure them.
    fn push_piece(&mut self, join_fp: i32, piece: &InkPiece, style: FontStyle) {
        match self {
            Self::BuiltIn(cursor) => cursor.push_piece(join_fp, piece, style),
            Self::Custom(cursor) => cursor.push_piece(join_fp, piece, style),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CustomLineInkCursor {
    advance_fp: i32,
    right: i16,
    settings: TypeSettings,
    style: FontStyle,
}

impl CustomLineInkCursor {
    const fn new(settings: TypeSettings, default_style: FontStyle) -> Self {
        Self {
            advance_fp: 0,
            right: 0,
            settings,
            style: default_style,
        }
    }

    fn reset_pair(&mut self) {}

    fn push_metric(&mut self, metric: display::font::GlyphMetric) {
        let advance = fixed_round(self.advance_fp);
        let glyph_right = advance + metric.x_offset as i16 + metric.width as i16;
        self.right = self.right.max(glyph_right);
        self.advance_fp += metric.advance_fp as i32;
    }

    fn push_fallback(&mut self) {
        self.advance_fp += 8 << 4;
        self.right = self.right.max(fixed_ceil(self.advance_fp));
    }

    fn push_object(&mut self, width: u16) {
        let left = fixed_round(self.advance_fp);
        self.right = self.right.max(left.saturating_add(width as i16));
        self.advance_fp += i32::from(width) << 4;
    }

    fn push_piece(&mut self, join_fp: i32, piece: &InkPiece, style: FontStyle) {
        let end = self.advance_fp + join_fp + piece.advance_fp;
        self.right = self.right.max(piece.right(end));
        self.advance_fp = end;
        self.style = style;
    }

    fn width(&self) -> i16 {
        self.right.max(fixed_ceil(self.advance_fp))
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn reset_line_ink(&mut self) {
        self.line_ink = LineInkCursor::new(self.library.type_settings(), FontStyle::Regular);
    }

    fn push_line_ink_str(&mut self, text: &str) {
        match &mut self.line_ink {
            LineInkCursor::BuiltIn(cursor) => cursor.push_str(text),
            LineInkCursor::Custom(cursor) => {
                crate::custom_font::for_each_metric(
                    self.root,
                    self.library,
                    &mut *self.font_metrics,
                    cursor.settings.size,
                    cursor.settings.weight,
                    cursor.style,
                    text,
                    |style, metric| {
                        if cursor.style != style {
                            cursor.reset_pair();
                            cursor.style = style;
                        }
                        if let Some(metric) = metric {
                            cursor.push_metric(metric);
                        } else {
                            cursor.push_fallback();
                        }
                    },
                );
            }
        }
    }

    fn line_ink_width(&self) -> i16 {
        self.line_ink.width()
    }

    /// The line in hand takes the first-line indent: it opens a paragraph,
    /// as it does while no line of its paragraph has flushed (the previous
    /// block still closes one), unless a drop cap opens it, which stands in
    /// the indent's place.
    fn line_takes_indent(&self) -> bool {
        !self.line_unindented
            && (self.library.block_count == 0
                || self
                    .library
                    .block_paragraph_end
                    .get(self.library.block_count.wrapping_sub(1))
                    .copied()
                    .unwrap_or(true))
    }

    fn rebuild_pages_if_dirty(&mut self) {
        if self.pages_dirty {
            #[cfg(feature = "bench-serial")]
            let _span = build_profile::Span::new(&build_profile::PAGES);
            reader_layout::extend_page_index(self.library);
            self.pages_dirty = false;
        }
    }

    pub(crate) fn finish_spine(&mut self, partial: bool) {
        self.finish_lines();
        self.flush_section(partial || self.stopped, false);
    }

    /// Ends the chapter's last paragraph: its last line, then any images
    /// still held back, which no text followed.
    pub(crate) fn finish_lines(&mut self) {
        flush_styled_preview_line(self, true);
        if !self.pending.is_empty() {
            self.commit_pending_blocks();
        }
    }

    fn flush_section(&mut self, partial: bool, carry_incomplete: bool) -> bool {
        // Its lines are written, or move to the arena's front.
        self.window.forget_lines();
        self.rebuild_pages_if_dirty();
        if self.library.block_count() == 0 || self.library.page_count == 0 {
            self.library.clear_lines();
            self.pages_dirty = true;
            return true;
        }
        let (section_id, start_page, full) = match self.run {
            SpineRun::Prefix => {
                let table = &self.library.book_sections;
                let (id, start) = table.next_slot(self.section_base, 0);
                (id, start, table.is_full())
            }
            SpineRun::Seed => {
                let table = &self.library.seed.sections;
                let (id, start) = table.next_slot(
                    self.section_base,
                    crate::reader_store::PROVISIONAL_PAGE_BASE,
                );
                // Provisional pages must stay u16 page requests.
                let room = start + MAX_READER_PAGES as u32 <= u32::from(u16::MAX);
                (id, start, table.is_full() || !room)
            }
        };
        if full {
            match self.run {
                SpineRun::Prefix => *self.book_partial = true,
                SpineRun::Seed => self.run_full = true,
            }
            self.stopped = true;
            return false;
        }
        if partial {
            *self.book_partial = true;
        }

        self.library.set_cached_spine(self.spine_index);
        self.library.set_section_partial(partial);
        let write_started = Instant::now();
        #[cfg(feature = "bench-serial")]
        let reads_before = crate::sd_session::sd_stats::snapshot().read_blocks;
        let mut written_pages = 0;
        let wrote = reader_layout::flush_page_window(self.library, carry_incomplete, |window| {
            written_pages = window.page_count;
            match self.sections_dir {
                Some(sections) => reader_cache_files::write_v2_section_cache_in(
                    sections,
                    self.source_identity,
                    section_id,
                    window,
                ),
                None => false,
            }
        });
        *self.write_micros += write_started.elapsed().as_micros();
        #[cfg(feature = "bench-serial")]
        build_profile::SECTION_WRITE_READS.fetch_add(
            crate::sd_session::sd_stats::snapshot()
                .read_blocks
                .wrapping_sub(reads_before),
            portable_atomic::Ordering::Relaxed,
        );
        if !wrote {
            self.write_failed = true;
            self.stopped = true;
            return false;
        }
        let record = BookV2SectionRecord {
            section: section_id,
            spine: self.spine_index,
            start_page,
            page_count: written_pages.min(u16::MAX as usize) as u16,
            partial,
        };
        let pushed = match self.run {
            SpineRun::Prefix => self.library.book_sections.push(record),
            SpineRun::Seed => self.library.seed.sections.push(record),
        };
        if !pushed {
            match self.run {
                SpineRun::Prefix => *self.book_partial = true,
                SpineRun::Seed => self.run_full = true,
            }
            self.stopped = true;
            return false;
        }
        self.pages_dirty = false;
        true
    }

    fn flush_if_full(&mut self) {
        self.rebuild_pages_if_dirty();
        if self.library.page_count >= self.target_pages
            || self.library.block_count() >= self.library.block_capacity().saturating_sub(4)
            || self.library.text_capacity_reached()
        {
            // The line still being filled is not pushed here: the section
            // carries its unfinished last page forward, and the line keeps
            // collecting words until it wraps naturally. Pushing it early
            // ended a line mid-paragraph at most section boundaries. A
            // line awaiting repair has it first, while it can be taken
            // back.
            if self.window.repair_pending() {
                repair_window(self, End::Window);
            }
            self.flush_section(false, true);
        }
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> XhtmlBlockSink
    for LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn push_block(
        &mut self,
        text: &str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
        paragraph_end: bool,
    ) -> Result<(), XhtmlError> {
        #[cfg(feature = "bench-serial")]
        let _span = build_profile::Span::new(&build_profile::PARAGRAPHS);
        if self.stopped || self.interrupted {
            return Err(XhtmlError::TooManyRuns);
        }
        if self.preparation.gives_way() {
            self.interrupted = true;
            return Err(XhtmlError::TooManyRuns);
        }
        if self.unprobed.is_some() {
            // This pass is laid out again once its images are probed.
            return Ok(());
        }
        // A book with images: its paragraphs can hold images in their
        // lines, which the run may place first, or the paragraph's end
        // send out as blocks.
        let with_images = self.images.is_some();
        let text = match with_images {
            false => text,
            // The images held go first, each from this small frame, so
            // none of them rides under another.
            true => match self.ahead_of_text(text, role, style, align) {
                Ahead::Text(text) => text,
                Ahead::PlaceHeld(text, line_align) => {
                    self.place_pending_inline(role, line_align);
                    text
                }
                Ahead::CommitHeld(text) => {
                    self.commit_pending_blocks();
                    text
                }
                Ahead::Spacing if paragraph_end => "",
                Ahead::Spacing => return Ok(()),
            },
        };
        self.flush_if_full();
        push_styled_preview_fragment(
            self,
            text,
            preview_style_for_proto_style(style, role),
            role,
            align,
            paragraph_end,
            Fragment::Text,
        );
        self.flush_if_full();
        if with_images && paragraph_end {
            self.end_paragraph_with_images();
        }
        Ok(())
    }

    fn push_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
        if self.images.is_none() {
            return push_image_placeholder(self, alt);
        }
        self.push_image_in_paragraph(src, alt, false)
    }

    fn push_floated_image(&mut self, src: &str, alt: Option<&str>) -> Result<(), XhtmlError> {
        if self.images.is_none() {
            return push_image_placeholder(self, alt);
        }
        self.push_image_in_paragraph(src, alt, true)
    }

    fn push_svg_image(&mut self, href: &str) -> Result<(), XhtmlError> {
        if !self.pending.is_empty() {
            // Only a drop cap carried ahead is still held: the parser ended
            // the paragraph before the svg. An image follows it, so it
            // stays the block it was.
            self.commit_pending_blocks();
        }
        match self.look_up_image(href) {
            Lookup::Stop => Err(XhtmlError::TooManyRuns),
            Lookup::Shown {
                slot,
                block: (width, height),
                ..
            } => {
                let token =
                    |out: &mut [u8]| encode_image_token(slot, width, None, out).map(str::len);
                if !self.push_image_block(height, token) && self.stopped {
                    return Err(XhtmlError::TooManyRuns);
                }
                Ok(())
            }
            // An svg image never had a placeholder.
            Lookup::Missing | Lookup::Skip => Ok(()),
        }
    }

    /// Only a book with an image index hears of svg images: the parser ends
    /// the paragraph around each, which must not change a book without.
    fn wants_svg_images(&self) -> bool {
        self.images.is_some()
    }

    /// Only a book with an image index keeps images in their paragraphs, so
    /// a book without parses as it always did.
    fn wants_inline_images(&self) -> bool {
        self.images.is_some()
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    /// What a run of text in a book with images does first, while images
    /// are held back at its paragraph's start: text places them inside its
    /// line, ahead of it; a run of spacing is held in order with them; a
    /// run the sanitizer will drop (a title-page label, Gutenberg's
    /// metadata) is no text, and leaves them held. Decides only, out of
    /// line: the caller does the placing, so neither this frame nor a book
    /// without images rides under the text path's deepest frames.
    #[inline(never)]
    fn ahead_of_text<'t>(
        &mut self,
        text: &'t str,
        role: TextRole,
        style: proto::text::FontStyle,
        align: TextAlign,
    ) -> Ahead<'t> {
        if self.pending.is_empty() || self.dropping_paragraph || text.is_empty() {
            return Ahead::Text(text);
        }
        if !has_visible_text(text) {
            let spacing = PendingImage {
                held: Held::Spacing {
                    role,
                    style: preview_style_for_proto_style(style, role),
                    align,
                },
                leading_space: false,
                drop_cap: false,
            };
            let write = |out: &mut [u8]| {
                let room = out.get_mut(..text.len())?;
                room.copy_from_slice(text.as_bytes());
                Some(text.len())
            };
            return match self.pending.push(spacing, write) {
                true => Ahead::Spacing,
                false => Ahead::CommitHeld(text),
            };
        }
        if !self.will_place_text(text) {
            return Ahead::Text(text);
        }
        // The alignment the run will take: a typed scene break is centred.
        let align = block_align_for(align, text, role);
        if self.pending.carried
            && (role != TextRole::Body || align == TextAlign::Center || !starts_with_letter(text))
        {
            // A drop cap is the first letter of a word of body text: before
            // a heading, a centred line or scene break, a list item's
            // marker or anything else, it stays a block.
            return Ahead::CommitHeld(text);
        }
        let line_align = reader_layout::effective_align(align, self.library.type_settings());
        // A drop cap is the first letter of the word after it.
        let text = match self.pending.carried {
            true => text.trim_start_matches(|ch: char| ch.is_ascii_whitespace()),
            false => text,
        };
        Ahead::PlaceHeld(text, line_align)
    }

    /// Whether a run of text that is not all whitespace will be placed:
    /// the sanitizer keeps it. Normalized and checked as the line builder
    /// will, on a frame of its own beside the line builder's, not under it.
    #[inline(never)]
    fn will_place_text(&self, text: &str) -> bool {
        let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
        push_normalized(text, &mut normalized, true);
        trim_trailing_space(&mut normalized);
        sanitize_preview_block(&mut normalized) && normalized.chars().any(|ch| !ch.is_whitespace())
    }

    /// A paragraph of a book with images ended: images held back since its
    /// start, with no text after them, go out as blocks, unless a lone
    /// drop cap carries into the next paragraph.
    fn end_paragraph_with_images(&mut self) {
        self.paragraph_has_text = false;
        if !self.pending.is_empty() {
            self.end_pending_paragraph();
        }
    }

    /// An `<img>` in a book with images: inside its line when its paragraph
    /// already has text, otherwise held back until the paragraph shows
    /// whether text follows. `floated` says its block floats left. The
    /// lookup, the hold and the placement are siblings under this small
    /// frame, so none rides under another.
    fn push_image_in_paragraph(
        &mut self,
        src: &str,
        alt: Option<&str>,
        floated: bool,
    ) -> Result<(), XhtmlError> {
        let shown = match self.look_up_image(src) {
            Lookup::Stop => return Err(XhtmlError::TooManyRuns),
            Lookup::Skip => return Ok(()),
            Lookup::Missing => None,
            Lookup::Shown {
                slot,
                block,
                inline: None,
                ..
            } => return self.push_illustration(slot, block, alt),
            Lookup::Shown {
                slot,
                block,
                inline: Some(inline),
                inline_marked,
            } => Some(ShownImage {
                slot,
                block,
                inline,
                inline_marked,
            }),
        };
        match self.hold_image(shown, alt, floated) {
            Hold::Pending => {}
            Hold::Inline => self.place_pending_inline(self.line_role, self.line_align),
            Hold::Block(shown) => {
                // No room to hold it: the block it would be.
                let token = |out: &mut [u8]| {
                    encode_image_token(shown.slot, shown.block.0, alt, out).map(str::len)
                };
                if !self.push_image_block(shown.block.1, token) && !self.stopped {
                    return push_image_placeholder(self, alt);
                }
            }
            Hold::Placeholder => return push_image_placeholder(self, alt),
        }
        if self.stopped {
            return Err(XhtmlError::TooManyRuns);
        }
        Ok(())
    }

    /// An illustration, too tall to go inside a line: a block, as it always
    /// was, ending the paragraph around it even when that has text. Images
    /// held at the paragraph's start go out first, as blocks; text after it
    /// starts a paragraph of its own.
    #[inline(never)]
    fn push_illustration(
        &mut self,
        slot: u16,
        (width, height): (u16, u16),
        alt: Option<&str>,
    ) -> Result<(), XhtmlError> {
        if !self.pending.is_empty() {
            self.commit_pending_blocks();
        }
        flush_styled_preview_line(self, true);
        self.paragraph_has_text = false;
        let token = |out: &mut [u8]| encode_image_token(slot, width, alt, out).map(str::len);
        if !self.push_image_block(height, token) {
            if self.stopped {
                return Err(XhtmlError::TooManyRuns);
            }
            // No room even in a fresh arena: the alt text, rather than
            // nothing.
            return push_image_placeholder(self, alt);
        }
        Ok(())
    }

    /// Holds an image back with what it needs as a block: its token, or the
    /// alt text standing in for one that cannot be shown (an image going
    /// inside a line at once needs only the latter). Each must fit whole.
    #[inline(never)]
    fn hold_image(&mut self, shown: Option<ShownImage>, alt: Option<&str>, floated: bool) -> Hold {
        if self.pending.carried {
            // An image, not text, follows the drop cap carried ahead: it
            // stays the block it was.
            self.commit_pending_blocks();
        }
        // Text a dropped paragraph placed before it does not count: the
        // rest of that paragraph is gone, and the image stays what it was.
        let inline_now = self.paragraph_has_text && !self.dropping_paragraph;
        let record = PendingImage {
            held: shown.map_or(Held::Missing, Held::Shown),
            leading_space: core::mem::take(&mut self.pending_space),
            drop_cap: floated && !inline_now && alt.is_some_and(alt_is_initial),
        };
        let write = |out: &mut [u8]| match shown {
            Some(_) if inline_now => Some(0),
            Some(shown) => {
                let room = out.get_mut(..IMAGE_TOKEN_MAX_BYTES)?;
                encode_image_token(shown.slot, shown.block.0, alt, room).map(str::len)
            }
            None => Some(image_alt_text(alt, out.get_mut(..HELD_ALT_BYTES)?)),
        };
        if !self.pending.push(record, write) {
            // A long run of images before any text: those held go out as
            // blocks, as they would if the paragraph ended here.
            self.commit_pending_blocks();
            if !self.pending.push(record, write) {
                return match shown {
                    Some(shown) => Hold::Block(shown),
                    None => Hold::Placeholder,
                };
            }
        }
        if inline_now {
            Hold::Inline
        } else {
            Hold::Pending
        }
    }

    /// Looks the image up in the book's index. Out of line: the lookup's
    /// buffers never ride under the paragraph text that may follow it,
    /// which takes the text path's deepest frames.
    #[inline(never)]
    fn look_up_image(&mut self, src: &str) -> Lookup {
        let Some(images) = self.images else {
            return Lookup::Missing;
        };
        if self.stopped || self.interrupted {
            return Lookup::Stop;
        }
        if self.preparation.gives_way() {
            self.interrupted = true;
            return Lookup::Stop;
        }
        // Every pass meets the spine's images in the same order, whatever
        // the layout, so this is the image's place in reading order.
        let at = ImagePlace {
            spine: self.spine_index,
            order: self.images_met,
        };
        self.images_met = self.images_met.saturating_add(1);
        let placement = images.place(
            self.chapter,
            src,
            &mut self.image_path[..],
            self.library.portrait(),
            self.first_pass,
            at,
        );
        match placement {
            Err(()) => {
                self.write_failed = true;
                self.stopped = true;
                Lookup::Stop
            }
            Ok(ImagePlacement::Unprobed { slot }) => {
                self.unprobed = Some(match self.unprobed {
                    Some((first, last)) => (first.min(slot), last.max(slot)),
                    None => (slot, slot),
                });
                Lookup::Skip
            }
            // Laid out again after the probe: only unprobed images matter.
            Ok(_) if self.unprobed.is_some() => Lookup::Skip,
            Ok(ImagePlacement::Missing) => Lookup::Missing,
            Ok(ImagePlacement::Shown {
                slot,
                block,
                inline,
                inline_marked,
            }) => Lookup::Shown {
                slot,
                block,
                inline,
                inline_marked,
            },
        }
    }

    /// Places the records held back inside the line, in order: each image
    /// at its inline size, in place of each that cannot be shown its alt
    /// text in italics, and the spacing among them. `role` and `align` are
    /// the paragraph's.
    #[inline(never)]
    fn place_pending_inline(&mut self, role: TextRole, align: TextAlign) {
        let mut pending = core::mem::take(&mut self.pending);
        // A drop cap starts its word: no space after it.
        let carried = pending.carried;
        if carried && self.line.is_empty() {
            self.line_unindented = true;
        }
        for (record, text) in pending.iter() {
            if self.stopped {
                break;
            }
            match record.held {
                Held::Shown(shown) => {
                    let token = InlineImageToken {
                        slot: shown.slot,
                        width: shown.inline.0,
                        height: shown.inline.1,
                    };
                    let space = record.leading_space | core::mem::take(&mut self.pending_space);
                    place_inline_image(self, token, space, role, align);
                    if shown.inline != shown.block && !shown.inline_marked {
                        self.mark_inline(shown.slot);
                    }
                }
                Held::Missing => {
                    let text = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
                    if !text.is_empty() {
                        self.pending_space |= record.leading_space;
                        push_styled_preview_fragment(
                            self,
                            text,
                            FontStyle::Italic,
                            role,
                            align,
                            false,
                            Fragment::Alt,
                        );
                    }
                }
                Held::Spacing { style, .. } => {
                    push_styled_preview_fragment(
                        self,
                        text,
                        style,
                        role,
                        align,
                        false,
                        Fragment::Text,
                    );
                }
            }
        }
        if carried {
            self.pending_space = false;
        }
        pending.clear();
        self.pending = pending;
    }

    /// Sends the records held back out as what they are alone in a
    /// paragraph, exactly as they always were: image blocks, alt-text
    /// placeholders for images that cannot be shown, and their spacing as
    /// the lines it made.
    #[inline(never)]
    fn commit_pending_blocks(&mut self) {
        let mut pending = core::mem::take(&mut self.pending);
        for (record, text) in pending.iter() {
            if self.stopped || self.interrupted {
                break;
            }
            let (text, style, role, align) = match record.held {
                Held::Shown(shown) => {
                    let token = |out: &mut [u8]| {
                        let room = out.get_mut(..text.len())?;
                        room.copy_from_slice(text.as_bytes());
                        Some(text.len())
                    };
                    if self.push_image_block(shown.block.1, token) || self.stopped {
                        continue;
                    }
                    // No room even in a fresh arena: the alt text, rather
                    // than nothing.
                    let alt = decode_image_token(text).map_or("", |token| token.alt);
                    (alt, FontStyle::Italic, TextRole::Body, TextAlign::Center)
                }
                Held::Missing => (text, FontStyle::Italic, TextRole::Body, TextAlign::Center),
                Held::Spacing { role, style, align } => (text, style, role, align),
            };
            self.flush_if_full();
            push_styled_preview_fragment(self, text, style, role, align, true, Fragment::Text);
            self.flush_if_full();
        }
        pending.clear();
        self.pending = pending;
    }

    /// A paragraph that held images ended without text after them. A lone
    /// drop cap carries into the next paragraph (it stays held through the
    /// empty ones between); anything else goes out as blocks.
    fn end_pending_paragraph(&mut self) {
        if self.pending.fresh == 0 && self.pending.carried {
            return;
        }
        if self.pending.holds_lone_drop_cap() {
            self.pending.carried = true;
            self.pending.fresh = 0;
            return;
        }
        self.commit_pending_blocks();
    }

    /// Marks the image in `slot` as shown inside a line at its own size,
    /// once: it decodes at that size from now on.
    fn mark_inline(&mut self, slot: u16) {
        let Some(images) = self.images else {
            return;
        };
        match images.mark_inline(slot) {
            // A decode at its block size was forgotten: what the store
            // noted as settled may need decoding again.
            Ok(true) => self.library.forget_settled_images(),
            Ok(false) => {}
            Err(()) => {
                self.write_failed = true;
                self.stopped = true;
            }
        }
    }

    /// An image block `height` rows tall: its own paragraph, whose token
    /// `write_token` writes straight into the store's text arena. False
    /// when there is no room for it, or the spine stopped.
    fn push_image_block(
        &mut self,
        height: u16,
        write_token: impl Fn(&mut [u8]) -> Option<usize>,
    ) -> bool {
        // The paragraph ended before the image.
        if !self.line.is_empty() {
            flush_styled_preview_line(self, true);
        }
        self.window.clear();
        self.flush_if_full();
        if self.stopped {
            return false;
        }
        let spine = self.spine_index;
        if !self.library.push_image_block(height, spine, &write_token) {
            // The arena filled: flush and retry in a fresh one, as a line
            // does.
            self.flush_section(false, true);
            if self.stopped || !self.library.push_image_block(height, spine, &write_token) {
                return false;
            }
        }
        self.pages_dirty = true;
        self.hyphen_run = 0;
        self.flush_if_full();
        true
    }
}

/// Whether a run's first visible character is a letter, as the rest of a
/// drop cap's word is.
fn starts_with_letter(text: &str) -> bool {
    text.chars()
        .find(|&ch| !ch.is_whitespace() && !is_invisible_format_char(ch))
        .is_some_and(char::is_alphabetic)
}

/// Whether a run of text holds anything but whitespace: an image in the
/// same paragraph then goes inside its line.
fn has_visible_text(text: &str) -> bool {
    text.chars()
        .any(|ch| !ch.is_whitespace() && !is_invisible_format_char(ch) && ch != IMAGE_TOKEN_MARK)
}

fn preview_style_for_proto_style(style: proto::text::FontStyle, role: TextRole) -> FontStyle {
    match style {
        proto::text::FontStyle::BoldItalic => FontStyle::BoldItalic,
        proto::text::FontStyle::Bold => FontStyle::Bold,
        proto::text::FontStyle::Italic => FontStyle::Italic,
        proto::text::FontStyle::Regular => {
            if matches!(
                role,
                TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
            ) {
                FontStyle::Bold
            } else {
                FontStyle::Regular
            }
        }
    }
}

/// What a run of text pushed into the line builder is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fragment {
    /// The book's text.
    Text,
    /// The alt text standing in for an image that cannot be shown inside a
    /// line: kept whatever it says (it is not checked for title-page
    /// labels or separators, which would drop or recentre the paragraph),
    /// at the paragraph's alignment.
    Alt,
}

fn push_styled_preview_fragment<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    text: &str,
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    paragraph_end: bool,
    fragment: Fragment,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.dropping_paragraph {
        if paragraph_end {
            sink.dropping_paragraph = false;
            sink.pending_space = false;
        }
        return;
    }
    let starts_with_space = text
        .chars()
        .next()
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false);
    let ends_with_space = text
        .chars()
        .next_back()
        .map(|ch| ch.is_ascii_whitespace())
        .unwrap_or(false);
    let mut normalized = String::<MAX_READER_BLOCK_TEXT>::new();
    // A book with images writes image tokens into its lines, so its own
    // text loses the mark a token starts with.
    push_normalized(text, &mut normalized, sink.images.is_some());
    trim_trailing_space(&mut normalized);
    // A fragment with no visible text only carries spacing. Handle it before
    // the paragraph sanitizer, which treats an empty block as "drop the rest
    // of this paragraph".
    if normalized.is_empty() {
        sink.pending_space |= starts_with_space || ends_with_space;
        if paragraph_end {
            flush_styled_preview_line(sink, true);
        }
        return;
    }
    let align = if fragment == Fragment::Alt {
        align
    } else {
        if !sanitize_preview_block(&mut normalized) {
            sink.dropping_paragraph = !paragraph_end;
            sink.pending_space = false;
            return;
        }
        normalize_decorative_separator(&mut normalized);
        if sink.images.is_some() && normalized.chars().any(|ch| !ch.is_whitespace()) {
            // Text, not just spacing: an image met in this paragraph from
            // now on goes inside its line.
            sink.paragraph_has_text = true;
        }
        reader_layout::effective_align(
            block_align_for(align, normalized.as_str(), role),
            sink.library.type_settings(),
        )
    };
    let page_box = sink.library.page_box();
    let x = page_box.x_for(role);
    let max_x = page_box.right;
    // Book-style first-line indent: a Body paragraph's opening line wraps
    // against a narrower column. Only Left/Justify Body takes it, matching
    // `ui::reading::block_first_line_indent`, so the built line breaks agree
    // with how the page later draws them.
    let indent = if matches!(role, TextRole::Body)
        && matches!(align, TextAlign::Left | TextAlign::Justify)
    {
        reader_layout::paragraph_indent(sink.library.type_settings().size)
    } else {
        0
    };

    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_styled_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }

    // Hyphenation fills justified prose lines; headings, centred and ragged
    // text break only where a word is wider than the whole line.
    let justified_body = matches!(role, TextRole::Body) && align == TextAlign::Justify;
    let mut first_word = true;
    // ASCII whitespace only: a no-break space joins its neighbours into one
    // unbreakable word.
    let mut words = normalized.split_ascii_whitespace().peekable();
    while let Some(word) = words.next() {
        let spaced = sink.pending_space || !first_word || starts_with_space;
        // A paragraph's last word is never split: its tail would sit alone
        // on the last line.
        let may_hyphenate = justified_body && !(paragraph_end && words.peek().is_none());
        if justified_body {
            // First, as it may take back the lines before it.
            let space = spaced && !is_leading_punctuation_word(word);
            let placement = WindowPlacement {
                role,
                align,
                x,
                indent,
                max_x,
            };
            window_word(sink, word, style, space, may_hyphenate, placement);
        }
        let attach = is_leading_punctuation_word(word) && !sink.line.is_empty();
        let mut leading_space = !sink.line.is_empty() && !attach && spaced;
        let mut piece = word;
        while let Placement::Continue(rest) = place_word_piece(
            sink,
            piece,
            WordPlacement {
                style,
                role,
                align,
                leading_space,
                may_hyphenate,
                x,
                indent,
                max_x,
            },
        ) {
            piece = rest;
            leading_space = false;
        }
        first_word = false;
    }

    // A run ending in a space owes the next run (the next styled span, or
    // the next chunk the parser streams) that space.
    sink.pending_space |= ends_with_space;
    if paragraph_end {
        flush_styled_preview_line(sink, true);
    }
}

/// Character boundaries measured for one word at a line end; a head never
/// holds more letters than a line does.
const HEAD_WIDTH_SLOTS: usize = 64;

/// Where build time goes, for the bench: each span adds its wall time to a
/// counter (spans nest: a paragraph's time includes its lines' and pages').
#[cfg(feature = "bench-serial")]
pub(crate) mod build_profile {
    use portable_atomic::{AtomicU32, Ordering};
    pub(crate) static PARAGRAPHS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static LINES: AtomicU32 = AtomicU32::new(0);
    pub(crate) static PAGES: AtomicU32 = AtomicU32::new(0);
    pub(crate) static COMMITS: AtomicU32 = AtomicU32::new(0);
    /// A spine's whole read: inflating, parsing, and laying out.
    pub(crate) static SPINE_READS: AtomicU32 = AtomicU32::new(0);
    /// Parsing and laying out, fed inflated windows.
    pub(crate) static FEEDS: AtomicU32 = AtomicU32::new(0);
    /// Block reads made while writing sections (FAT and partial blocks).
    pub(crate) static SECTION_WRITE_READS: AtomicU32 = AtomicU32::new(0);

    pub(crate) struct Span(&'static AtomicU32, embassy_time::Instant);

    impl Span {
        pub(crate) fn new(counter: &'static AtomicU32) -> Self {
            Self(counter, embassy_time::Instant::now())
        }
    }

    impl Drop for Span {
        fn drop(&mut self) {
            self.0
                .fetch_add(self.1.elapsed().as_micros() as u32, Ordering::Relaxed);
        }
    }

    pub(crate) fn take_ms(counter: &AtomicU32) -> u32 {
        counter.swap(0, Ordering::Relaxed) / 1000
    }
}

#[cfg(feature = "bench-serial")]
pub(crate) static HYPHEN_LOOKUPS: portable_atomic::AtomicU32 = portable_atomic::AtomicU32::new(0);
#[cfg(feature = "bench-serial")]
pub(crate) static HYPHEN_LOOKUP_MICROS: portable_atomic::AtomicU32 =
    portable_atomic::AtomicU32::new(0);

/// Where the line repair's time goes, for the bench: lookups the break
/// cache answered, repairs made, pieces the window measured, and candidate
/// lines the search weighed (its time less its lookups and pieces). Apart
/// from repairs: keeping words and lines in the window, and measuring the
/// words it keeps (which the builder measures anyway).
#[cfg(feature = "bench-serial")]
pub(crate) mod repair_profile {
    use portable_atomic::{AtomicU32, Ordering};
    pub(crate) static KEEP_MICROS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static MEASURE_MICROS: AtomicU32 = AtomicU32::new(0);

    pub(crate) fn add_keep(started: embassy_time::Instant) {
        KEEP_MICROS.fetch_add(started.elapsed().as_micros() as u32, Ordering::Relaxed);
    }

    pub(crate) static LOOKUP_HITS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static REPAIRS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static REPAIR_MICROS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static PIECES: AtomicU32 = AtomicU32::new(0);
    pub(crate) static PIECE_MICROS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static CANDIDATES: AtomicU32 = AtomicU32::new(0);
    pub(crate) static RELAX_MICROS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static SEARCH_LOOKUPS: AtomicU32 = AtomicU32::new(0);
    pub(crate) static SEARCH_LOOKUP_MICROS: AtomicU32 = AtomicU32::new(0);
}

/// Where a word piece went: onto the current line, or (its tail, or the
/// whole piece after a line break) still waiting for the next line.
enum Placement<'w> {
    Done,
    Continue(&'w str),
}

#[derive(Clone, Copy)]
struct WordPlacement {
    style: FontStyle,
    role: TextRole,
    align: TextAlign,
    leading_space: bool,
    may_hyphenate: bool,
    x: i16,
    indent: i16,
    max_x: i16,
}

/// Puts `piece` (a word, or the tail of one split at the last line end) on
/// the line. When it does not fit, a line worth hyphenating takes the
/// longest head of it that does; otherwise the line flushes and the piece
/// waits for the next. A piece wider than a whole line breaks wherever it
/// can, and overflows only when it cannot. Out of line: its temporaries
/// must not ride the EPUB build's deepest frame.
#[inline(never)]
fn place_word_piece<
    'w,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    piece: &'w str,
    placement: WordPlacement,
) -> Placement<'w>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let WordPlacement {
        style,
        role,
        align,
        leading_space,
        may_hyphenate,
        x,
        indent,
        max_x,
    } = placement;
    let kept_len = sink.line.len();
    let kept_ink = sink.line_ink;
    let line_was_empty = sink.line.is_empty();
    if append_styled_word(&mut sink.line, piece, style, sink.line_style, leading_space).is_err() {
        // The line's text buffer is full: the piece starts the next line.
        sink.line.truncate(kept_len);
        flush_styled_preview_line(sink, false);
        if line_was_empty {
            return Placement::Done;
        }
        return Placement::Continue(piece);
    }
    let mut measure = String::<MAX_READER_BLOCK_TEXT>::new();
    let windowed = sink.window.keeps(role, align);
    if windowed {
        push_window_piece(sink, piece, style, leading_space, line_was_empty);
    } else {
        let _ = measure.push_str(&sink.line[kept_len..]);
        sink.push_line_ink_str(measure.as_str());
    }

    // The line in progress opens a paragraph while no line of it has
    // flushed yet: the previous block still closes a paragraph. Once the
    // first line flushes (as a non-end block) this goes false and the
    // continuation lines wrap at the full width.
    let x_eff = if sink.line_takes_indent() {
        x + indent
    } else {
        x
    };
    let limit = max_x - reader_layout::READER_WRAP_SAFETY;
    if sink.line_ink_width() + x_eff <= limit {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = style;
        sink.pending_space = false;
        return Placement::Done;
    }

    let worth_splitting = line_was_empty
        || may_hyphenate && {
            let kept_width = match kept_ink {
                LineInkCursor::BuiltIn(cursor) => cursor.width(),
                LineInkCursor::Custom(cursor) => cursor.width(),
            };
            let slack = limit - (x_eff + kept_width);
            // The word's own average letter, from the width just measured: a
            // line end with room for under about three of them (two letters
            // and a hyphen) cannot take a head, so it skips the glyph-by-glyph
            // measure and the pattern lookup.
            let piece_width = sink.line_ink_width() - kept_width;
            let average = piece_width / piece.chars().count().max(1) as i16;
            slack >= 3 * average
                && ui::reading::worth_hyphenating(
                    slack,
                    ui::reading::justify_gap_count(&sink.line[..kept_len]),
                    ui::reading::word_space_width(sink.library.type_settings(), style),
                    sink.hyphen_run,
                )
        };
    sink.line.truncate(kept_len);
    sink.line_ink = kept_ink;
    if worth_splitting {
        if let Some(rest) = split_word_at_line_end(sink, piece, kept_len, placement, x_eff, limit) {
            return Placement::Continue(rest);
        }
    }
    if line_was_empty {
        // Wider than any line and unbreakable: it overflows, as before.
        let _ = append_styled_word(&mut sink.line, piece, style, sink.line_style, leading_space);
        if windowed {
            push_window_piece(sink, piece, style, leading_space, true);
        } else {
            measure.clear();
            let _ = measure.push_str(sink.line.as_str());
            sink.push_line_ink_str(measure.as_str());
        }
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = style;
        sink.pending_space = false;
        return Placement::Done;
    }
    wrap_line(sink);
    Placement::Continue(piece)
}

/// Measures a justified word the window keeps, `text` from where it
/// resumes, onto the line in hand after what joins it to the glyph before
/// (none when `line_was_empty`): one measure serves the line and, for a
/// whole word, the window's search.
#[inline(never)]
fn push_window_piece<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    text: &str,
    style: FontStyle,
    space: bool,
    line_was_empty: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[cfg(feature = "bench-serial")]
    let _span = build_profile::Span::new(&repair_profile::MEASURE_MICROS);
    if line_was_empty {
        if let Some(piece) = sink.window.current_piece() {
            // The whole word again, moved to a new line.
            sink.line_ink.push_piece(0, &piece, style);
            return;
        }
    }
    let restyle = (style != sink.line_style).then_some(style);
    let (piece, join) = match &mut sink.line_ink {
        LineInkCursor::BuiltIn(cursor) => cursor.push_word(space, restyle, text),
        LineInkCursor::Custom(_) => {
            let mut measure =
                SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation);
            let piece = measure.piece(text, style, false);
            let previous = (sink.line_style, None);
            let join = match line_was_empty {
                true => 0,
                false => measure.join(previous, space, style, piece.first),
            };
            sink.line_ink.push_piece(join, &piece, style);
            (piece, join)
        }
    };
    let join = match sink.window.piece_joins() {
        // At a line's start the word joins the window's word before it.
        Some(previous) if line_was_empty => {
            let mut measure =
                SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation);
            measure.join(previous, sink.window.current_space(), style, piece.first)
        }
        _ => join,
    };
    #[cfg(feature = "bench-serial")]
    let started = Instant::now();
    sink.window.keep_measure(text.len(), &piece, join);
    #[cfg(feature = "bench-serial")]
    repair_profile::add_keep(started);
}

/// Puts an inline image on the line: one unbreakable unit of its fitted
/// width, after a space when the source had one and the line is not empty.
/// When it does not fit, the line flushes and the image starts the next;
/// an image is never split, and it fits any line on its own (its fitted
/// width allows for the widest indent). `role` and `align` are its
/// paragraph's. Out of line, like a word's placement.
#[inline(never)]
fn place_inline_image<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    image: InlineImageToken,
    mut leading_space: bool,
    role: TextRole,
    align: TextAlign,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !sink.line.is_empty() && (sink.line_role != role || sink.line_align != align) {
        flush_styled_preview_line(sink, false);
    }
    if sink.line.is_empty() {
        sink.line_role = role;
        sink.line_align = align;
        sink.line_style = FontStyle::Regular;
    }
    let page_box = sink.library.page_box();
    let indent = if matches!(role, TextRole::Body)
        && matches!(align, TextAlign::Left | TextAlign::Justify)
    {
        reader_layout::paragraph_indent(sink.library.type_settings().size)
    } else {
        0
    };
    let limit = page_box.right - reader_layout::READER_WRAP_SAFETY;
    let mut token = [0u8; INLINE_IMAGE_TOKEN_BYTES];
    let token = encode_inline_image_token(image, &mut token);
    if role == TextRole::Body && align == TextAlign::Justify {
        let placement = WindowPlacement {
            role,
            align,
            x: page_box.x_for(role),
            indent,
            max_x: page_box.right,
        };
        window_image(sink, token, image, leading_space, placement);
    }
    loop {
        let line_was_empty = sink.line.is_empty();
        let kept_len = sink.line.len();
        let kept_ink = sink.line_ink;
        let space = leading_space && !line_was_empty;
        let appended = (!space || sink.line.push(' ').is_ok()) && sink.line.push_str(token).is_ok();
        if appended {
            if space {
                sink.push_line_ink_str(" ");
            }
            sink.line_ink.push_object(image.width);
            let x_eff = page_box.x_for(role) + if sink.line_takes_indent() { indent } else { 0 };
            if line_was_empty || sink.line_ink_width() + x_eff <= limit {
                sink.line_image_height = sink.line_image_height.max(image.height);
                sink.pending_space = false;
                return;
            }
        }
        sink.line.truncate(kept_len);
        sink.line_ink = kept_ink;
        if line_was_empty {
            // Not even an empty line holds its token: nothing to place, and
            // the window holds what was not placed.
            sink.window.clear();
            return;
        }
        wrap_line(sink);
        leading_space = false;
    }
}

/// Ends the line with the longest head of `piece` that fits before `limit`
/// (plus its hyphen), flushes it, and returns where the piece resumes.
/// None, with the line as it was, when no break fits.
///
/// The piece is measured first, one glyph at a time, recording the head's
/// width at each character until it overruns the line: the glyph tables are
/// already cached from measuring every word, while the hyphenation patterns
/// are cold by the time a line ends, so they are only consulted when at
/// least a two-letter head and its hyphen fit, and only for that head.
#[inline(never)]
fn split_word_at_line_end<
    'w,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    piece: &'w str,
    kept_len: usize,
    placement: WordPlacement,
    x_eff: i16,
    limit: i16,
) -> Option<&'w str>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let kept_ink = sink.line_ink;
    // What `append_styled_word` puts before the word: its space and, when
    // the run changes style, the style marker.
    let mut lead = String::<4>::new();
    if placement.leading_space {
        let _ = lead.push(' ');
    }
    if placement.style != sink.line_style {
        let _ = append_style_marker(&mut lead, placement.style);
    }
    sink.push_line_ink_str(lead.as_str());
    let hyphen_width = {
        let before = sink.line_ink_width();
        let head_ink = sink.line_ink;
        sink.push_line_ink_str("-");
        let width = sink.line_ink_width() - before;
        sink.line_ink = head_ink;
        width
    };
    // Head width (with its hyphen) at each character boundary that still
    // fits; `fits_to` ends up at the longest head that does.
    let mut head_widths = heapless::Vec::<(u16, i16), HEAD_WIDTH_SLOTS>::new();
    let mut fits_to = 0;
    let mut encoded = [0u8; 4];
    for (index, ch) in piece.char_indices() {
        let with_hyphen = sink.line_ink_width() + hyphen_width;
        if with_hyphen + x_eff > limit || head_widths.is_full() {
            break;
        }
        let _ = head_widths.push((index as u16, with_hyphen));
        fits_to = index;
        if ch != SOFT_HYPHEN {
            sink.push_line_ink_str(ch.encode_utf8(&mut encoded));
        }
    }
    sink.line_ink = kept_ink;
    if piece[..fits_to]
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .count()
        < 2
    {
        // Not even a two-letter head fits: nothing to look up.
        return None;
    }
    // Every break the word has, from the window's cache when it can: those
    // whose heads do not fit have no width below.
    let mut breaks = heapless::Vec::<WordBreak, MAX_WORD_BREAKS>::new();
    {
        let mut measure =
            SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation);
        sink.window.cached_breaks(piece, &mut measure, &mut breaks);
        if breaks.is_full() {
            // Too many to be sure of those that fit.
            measure.breaks_to(piece, fits_to, &mut breaks);
        }
    }
    // The longest break whose head fits; a dash break draws no hyphen.
    let item = breaks.iter().rev().copied().find(|item| {
        head_widths
            .iter()
            .find(|(offset, _)| usize::from(*offset) == item.head_end)
            .is_some_and(|(_, with_hyphen)| {
                let width = if item.hyphen {
                    *with_hyphen
                } else {
                    with_hyphen - hyphen_width
                };
                width + x_eff <= limit
            })
    })?;
    if append_styled_word(
        &mut sink.line,
        &piece[..item.head_end],
        placement.style,
        sink.line_style,
        placement.leading_space,
    )
    .is_err()
        || (item.hyphen && sink.line.push('-').is_err())
    {
        sink.line.truncate(kept_len);
        return None;
    }
    sink.line_role = placement.role;
    sink.line_align = placement.align;
    sink.line_style = placement.style;
    sink.pending_space = false;
    sink.line_hyphenated = item.hyphen;
    sink.window.advance_piece(item.tail_start);
    wrap_line(sink);
    Some(&piece[item.tail_start..])
}

fn append_styled_word<const N: usize>(
    line: &mut String<N>,
    word: &str,
    style: FontStyle,
    current_style: FontStyle,
    leading_space: bool,
) -> Result<(), ()> {
    if leading_space {
        line.push(' ').map_err(|_| ())?;
    }
    // Emit a style marker only when the run actually changes. Plain prose
    // (the bulk of a book) then carries no markers at all -- ~25-30% more
    // text per section and proportionally fewer chunks against the same
    // 16 KB arena. The draw and measure paths both key off the running
    // font, so a dropped redundant marker is a no-op; and because each line
    // opens in Regular (`ui::reading::line_opening_style`) and
    // `current_style` is reset to Regular on every flush, a continuation
    // line still re-marks a non-Regular opening word, while plain words
    // that open a line stay unmarked.
    if style != current_style {
        append_style_marker(line, style)?;
    }
    // Soft hyphens only mark where a word may break; the fonts would draw
    // them, so they never reach a stored line.
    for ch in word.chars().filter(|ch| *ch != SOFT_HYPHEN) {
        line.push(ch).map_err(|_| ())?;
    }
    Ok(())
}

fn append_style_marker<const N: usize>(line: &mut String<N>, style: FontStyle) -> Result<(), ()> {
    line.push(reader_layout::STYLE_MARKER).map_err(|_| ())?;
    line.push(reader_layout::style_marker_code(style))
        .map_err(|_| ())
}

/// Stores what is in hand as its paragraph ends, or as something else ends
/// its lines: the window's lines first, repaired when one awaits it, then
/// the line in hand. The window keeps nothing past it.
fn flush_styled_preview_line<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    paragraph_end: bool,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.window.repair_pending() {
        repair_window(
            sink,
            End::Final {
                paragraph: paragraph_end,
            },
        );
    }
    sink.window.clear();
    push_line_in_hand(sink, paragraph_end);
}

/// Where the window's lines are set: as the word placement sets them.
#[derive(Clone, Copy)]
struct WindowPlacement {
    role: TextRole,
    align: TextAlign,
    x: i16,
    indent: i16,
    max_x: i16,
}

/// Keeps a justified word about to be placed in the window, taking back
/// lines that await repair first. A window starts on an empty line in
/// hand; when it has no room, its paragraph keeps the lines it is given.
#[inline(never)]
fn window_word<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    word: &str,
    style: FontStyle,
    space: bool,
    may_hyphenate: bool,
    placement: WindowPlacement,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !open_window(sink, placement) {
        return;
    }
    #[cfg(feature = "bench-serial")]
    let started = Instant::now();
    let pushed = sink.window.push_word(word, style, space, may_hyphenate);
    #[cfg(feature = "bench-serial")]
    repair_profile::add_keep(started);
    if pushed {
        return;
    }
    if sink.window.repair_pending() {
        repair_window(sink, End::Window);
        if sink.window.push_word(word, style, space, may_hyphenate) {
            return;
        }
    }
    sink.window.clear();
}

/// Keeps an inline image about to be placed in the window, as a word.
#[inline(never)]
fn window_image<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    token: &str,
    image: InlineImageToken,
    space: bool,
    placement: WindowPlacement,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !open_window(sink, placement) {
        return;
    }
    let (width, height) = (image.width, image.height);
    if sink.window.push_image(token, width, height, space) {
        return;
    }
    if sink.window.repair_pending() {
        repair_window(sink, End::Window);
        if sink.window.push_image(token, width, height, space) {
            return;
        }
    }
    sink.window.clear();
}

/// Readies the window for a piece about to be placed: repairs the lines
/// that await it once enough follow, and begins a window on an empty line
/// in hand. False when the window does not keep this line.
fn open_window<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    placement: WindowPlacement,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.window.repair_due() {
        repair_window(sink, End::Window);
    }
    if sink.window.keeps(placement.role, placement.align) {
        return true;
    }
    if !sink.line.is_empty() {
        sink.window.clear();
        return false;
    }
    sink.window.begin(
        placement.role,
        placement.align,
        placement.x,
        placement.max_x,
        placement.indent,
    );
    sink.window.active()
}

/// Stores the line in hand where the word placement wrapped it, noting it
/// in the window with whether it wants repair.
#[inline(never)]
fn wrap_line<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if !sink.window.active() {
        push_line_in_hand(sink, false);
        return;
    }
    #[cfg(feature = "bench-serial")]
    let started = Instant::now();
    let bad = line_wants_repair(sink);
    #[cfg(feature = "bench-serial")]
    repair_profile::add_keep(started);
    let run_before = sink.hyphen_run;
    let unindented = sink.line_unindented;
    if push_line_in_hand(sink, false) {
        #[cfg(feature = "bench-serial")]
        let started = Instant::now();
        let block = sink.library.block_count() - 1;
        sink.window.note_line(block, run_before, bad, unindented);
        sink.window.trim();
        #[cfg(feature = "bench-serial")]
        repair_profile::add_keep(started);
    } else {
        sink.window.clear();
    }
}

/// Whether the line in hand, about to be stored, wants repair: the page
/// leaves it ragged (its gaps would open past two spaces, or it falls
/// short of two thirds of the measure), or it is a third hyphenated line
/// in a row.
fn line_wants_repair<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    if sink.line_hyphenated && sink.hyphen_run >= ui::reading::MAX_HYPHEN_RUN {
        return true;
    }
    let (left, right) = sink.window.edges();
    let x = left
        + if sink.line_takes_indent() {
            sink.window.indent()
        } else {
            0
        };
    let width = sink.line_ink_width();
    if x + width >= right - reader_layout::READER_WRAP_SAFETY {
        // Full, or a word too wide to fit: nothing to repair.
        return false;
    }
    let gaps = ui::reading::justify_gap_count(sink.line.as_str());
    if sink.line_image_height > 0 {
        return ui::reading::justify_inline_extra(x, width, right, gaps).is_none();
    }
    let opening = reader_layout::line_opening_style(sink.line.as_str(), FontStyle::Regular);
    let space = {
        let mut measure =
            SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation);
        sink.window.space(&mut measure, opening)
    };
    ui::reading::justify_extra(x, width, right, gaps, space).is_none()
}

/// Takes back the stored lines from a few before the first that wants
/// repair, with the line in hand, and breaks their words again: all the
/// lines but the last are stored, and the last stays in hand (to go on
/// filling when `end` is `End::Window`).
#[inline(never)]
fn repair_window<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    end: End,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[cfg(feature = "bench-serial")]
    let started = Instant::now();
    let Some(root) = sink.window.take_back() else {
        return;
    };
    sink.library.truncate_lines(root.block);
    sink.pages_dirty = true;
    sink.hyphen_run = root.run_before;
    sink.line_unindented = root.unindented;
    sink.line.clear();
    sink.reset_line_ink();
    sink.line_style = FontStyle::Regular;
    sink.line_image_height = 0;
    sink.line_hyphenated = false;
    // Its first line opens a paragraph while no line of it is stored.
    let first_indent = if sink.line_takes_indent() {
        sink.window.indent()
    } else {
        0
    };
    let lines = {
        let mut measure =
            SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation)
                .repairing();
        sink.window.measure(&mut measure);
        #[cfg(feature = "bench-serial")]
        let (searched, before) = (Instant::now(), measure.stats);
        let lines = sink.window.search(&mut measure, first_indent, end);
        #[cfg(feature = "bench-serial")]
        measure.searched(searched, before);
        lines
    };
    let mut node = 0;
    for index in 0..lines {
        let (span, to) = sink.window.line_from(node);
        node = to;
        set_window_line(sink, span);
        sink.window.set_hand_start((span.first, span.from));
        if index + 1 == lines {
            break;
        }
        let run_before = sink.hyphen_run;
        let unindented = sink.line_unindented;
        if push_line_in_hand(sink, false) {
            let block = sink.library.block_count() - 1;
            sink.window.note_line(block, run_before, false, unindented);
        }
    }
    if end == End::Window {
        remeasure_line_in_hand(sink);
    }
    sink.window.settle();
    #[cfg(feature = "bench-serial")]
    {
        use portable_atomic::Ordering;
        repair_profile::REPAIRS.fetch_add(1, Ordering::Relaxed);
        repair_profile::REPAIR_MICROS
            .fetch_add(started.elapsed().as_micros() as u32, Ordering::Relaxed);
    }
}

/// Builds the window's line `span` in hand, as the word placement would
/// have: a space where the source has one, a style marker where the run
/// changes style, soft hyphens dropped, image tokens, and a hyphen after a
/// broken head.
#[inline(never)]
fn set_window_line<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    span: Span,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    sink.line_role = sink.window.role;
    sink.line_align = sink.window.align;
    for index in span.first..=span.last {
        let word = sink.window.word(index);
        let text = sink.window.text(&word);
        let from = if index == span.first { span.from } else { 0 };
        let (to, hyphen) = match span.head {
            Some((head_end, hyphen)) if index == span.last => (head_end, hyphen),
            _ => (text.len(), false),
        };
        let lead = !sink.line.is_empty() && word.space && from == 0;
        if word.image {
            if (!lead || sink.line.push(' ').is_ok()) && sink.line.push_str(text).is_ok() {
                sink.line_image_height = sink.line_image_height.max(word.image_height());
            }
            continue;
        }
        let _ = append_styled_word(
            &mut sink.line,
            &text[from..to],
            word.style,
            sink.line_style,
            lead,
        );
        sink.line_style = word.style;
        if hyphen {
            let _ = sink.line.push('-');
        }
    }
    sink.line_hyphenated = matches!(span.head, Some((_, true)));
    sink.pending_space = false;
}

/// Measures the line in hand again, for word placement to go on filling
/// it after a repair left it there: its text, and each inline image as an
/// object of its width.
#[inline(never)]
fn remeasure_line_in_hand<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    sink.reset_line_ink();
    let mut measure =
        SinkMeasure::new(sink.root, sink.library, sink.font_metrics, sink.hyphenation);
    for piece in inline_pieces(sink.line.as_str()) {
        match piece {
            InlinePiece::Text(run) => measure.push(&mut sink.line_ink, run),
            InlinePiece::Image(image) => sink.line_ink.push_object(image.width),
        }
    }
}

/// What the window's measuring and lookups cost, for the bench.
#[cfg(feature = "bench-serial")]
#[derive(Clone, Copy, Default)]
struct MeasureStats {
    pieces: u32,
    piece_micros: u32,
    lookups: u32,
    lookup_micros: u32,
    hits: u32,
    candidates: u32,
}

/// Measures and looks up for the window as the line builder does: a
/// built-in face through its glyph and kerning tables, or the SD font
/// through its metric cache (which never kerns).
struct SinkMeasure<
    'm,
    'r,
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    library: &'m ReaderStore,
    metrics: &'m mut crate::custom_font::MetricCache,
    settings: TypeSettings,
    hyphenation: Hyphenation,
    custom: bool,
    /// An SD font's space advance by style, once measured: its whole join.
    space_advances: [i32; 4],
    /// Its pieces count as the window's own measuring (a repair's), not
    /// the builder's (which measures every word it places).
    repairing: bool,
    #[cfg(feature = "bench-serial")]
    stats: MeasureStats,
}

impl<'m, 'r, D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize>
    SinkMeasure<'m, 'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn new(
        root: &'r Directory<'r, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        library: &'m ReaderStore,
        metrics: &'m mut crate::custom_font::MetricCache,
        hyphenation: Hyphenation,
    ) -> Self {
        let settings = library.type_settings();
        Self {
            root,
            library,
            metrics,
            settings,
            hyphenation,
            // As `LineInkCursor::new` chooses.
            custom: settings.family == FontFamily::Custom
                && !display::font::builtin_custom_available(),
            space_advances: [0; 4],
            repairing: false,
            #[cfg(feature = "bench-serial")]
            stats: MeasureStats::default(),
        }
    }

    /// One for a repair, whose pieces the bench counts.
    fn repairing(mut self) -> Self {
        self.repairing = true;
        self
    }

    /// Pushes `text` into a line's running measure, as the builder
    /// measures the line in hand.
    fn push(&mut self, ink: &mut LineInkCursor, text: &str) {
        match ink {
            LineInkCursor::BuiltIn(cursor) => cursor.push_str(text),
            LineInkCursor::Custom(cursor) => {
                crate::custom_font::for_each_metric(
                    self.root,
                    self.library,
                    &mut *self.metrics,
                    cursor.settings.size,
                    cursor.settings.weight,
                    cursor.style,
                    text,
                    |style, metric| {
                        if cursor.style != style {
                            cursor.reset_pair();
                            cursor.style = style;
                        }
                        if let Some(metric) = metric {
                            cursor.push_metric(metric);
                        } else {
                            cursor.push_fallback();
                        }
                    },
                );
            }
        }
    }

    /// Where `word` may break with a head of at most `max_head` bytes.
    fn breaks_to(
        &mut self,
        word: &str,
        max_head: usize,
        out: &mut heapless::Vec<WordBreak, MAX_WORD_BREAKS>,
    ) {
        #[cfg(feature = "bench-serial")]
        let started = Instant::now();
        word_breaks(word, self.hyphenation, max_head, out);
        #[cfg(feature = "bench-serial")]
        {
            self.stats.lookups += 1;
            self.stats.lookup_micros += started.elapsed().as_micros() as u32;
        }
    }

    /// Adds the search's time, less its lookups and pieces, to the bench's
    /// count of time spent weighing candidates.
    #[cfg(feature = "bench-serial")]
    fn searched(&self, started: Instant, before: MeasureStats) {
        use portable_atomic::Ordering;
        let lookups = self.stats.lookup_micros - before.lookup_micros;
        let pieces = self.stats.piece_micros - before.piece_micros;
        let micros = started.elapsed().as_micros() as u32;
        repair_profile::RELAX_MICROS
            .fetch_add(micros.saturating_sub(lookups + pieces), Ordering::Relaxed);
        repair_profile::SEARCH_LOOKUPS
            .fetch_add(self.stats.lookups - before.lookups, Ordering::Relaxed);
        repair_profile::SEARCH_LOOKUP_MICROS.fetch_add(lookups, Ordering::Relaxed);
    }
}

#[cfg(feature = "bench-serial")]
impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> Drop
    for SinkMeasure<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn drop(&mut self) {
        use portable_atomic::Ordering;
        let stats = self.stats;
        HYPHEN_LOOKUPS.fetch_add(stats.lookups, Ordering::Relaxed);
        HYPHEN_LOOKUP_MICROS.fetch_add(stats.lookup_micros, Ordering::Relaxed);
        repair_profile::LOOKUP_HITS.fetch_add(stats.hits, Ordering::Relaxed);
        repair_profile::PIECES.fetch_add(stats.pieces, Ordering::Relaxed);
        repair_profile::PIECE_MICROS.fetch_add(stats.piece_micros, Ordering::Relaxed);
        repair_profile::CANDIDATES.fetch_add(stats.candidates, Ordering::Relaxed);
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize, const MAX_VOLUMES: usize> Measure
    for SinkMeasure<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    fn piece(&mut self, text: &str, style: FontStyle, hyphen: bool) -> InkPiece {
        #[cfg(feature = "bench-serial")]
        let started = Instant::now();
        let mut piece = InkPiece::default();
        if self.custom {
            for run in text.split(SOFT_HYPHEN).chain(hyphen.then_some("-")) {
                crate::custom_font::for_each_metric(
                    self.root,
                    self.library,
                    &mut *self.metrics,
                    self.settings.size,
                    self.settings.weight,
                    style,
                    run,
                    |_, metric| match metric {
                        Some(metric) => piece.push_metric(&metric),
                        None => piece.push_fallback(),
                    },
                );
            }
        } else {
            let font = ui::reading::body_font(self.settings, style);
            let chars = text.chars().filter(|ch| *ch != SOFT_HYPHEN);
            for ch in chars.chain(hyphen.then_some('-')) {
                piece.push_char(font, ch);
            }
        }
        #[cfg(feature = "bench-serial")]
        if self.repairing {
            self.stats.pieces += 1;
            self.stats.piece_micros += started.elapsed().as_micros() as u32;
        }
        piece
    }

    fn join(
        &mut self,
        (previous_style, previous): (FontStyle, Option<u16>),
        space: bool,
        style: FontStyle,
        first: Option<u16>,
    ) -> i32 {
        if !self.custom {
            return InkPiece::join_fp(self.settings, previous_style, previous, space, style, first);
        }
        if !space {
            return 0;
        }
        let index = usize::from(reader_layout::style_marker_code(previous_style) as u8 - b'0') & 3;
        if self.space_advances[index] == 0 {
            self.space_advances[index] = self.piece(" ", previous_style, false).advance_fp.max(1);
        }
        self.space_advances[index]
    }

    fn space(&mut self, style: FontStyle) -> i16 {
        if !self.custom {
            return ui::reading::word_space_width(self.settings, style);
        }
        // As the SD font's justified drawing takes it.
        let width = |piece: InkPiece| {
            piece
                .right(piece.advance_fp)
                .max(fixed_ceil(piece.advance_fp))
        };
        let wide = self.piece(" x", style, false);
        let narrow = self.piece("x", style, false);
        width(wide).saturating_sub(width(narrow))
    }

    fn breaks(&mut self, word: &str, out: &mut heapless::Vec<WordBreak, MAX_WORD_BREAKS>) {
        self.breaks_to(word, word.len(), out);
    }

    fn hit(&mut self) {
        #[cfg(feature = "bench-serial")]
        {
            self.stats.hits += 1;
        }
    }

    fn weigh(&mut self) {
        #[cfg(feature = "bench-serial")]
        {
            self.stats.candidates += 1;
        }
    }
}

/// Stores the line in hand (or, when it is empty, marks the paragraph's
/// end on the last block stored); true when it stored a line.
#[inline(never)]
fn push_line_in_hand<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    paragraph_end: bool,
) -> bool
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    #[cfg(feature = "bench-serial")]
    let _span = build_profile::Span::new(&build_profile::LINES);
    if sink.line.is_empty() {
        if paragraph_end {
            sink.hyphen_run = 0;
            sink.library.mark_last_block_paragraph_end();
            // Height-only change: the trailing paragraph gap moved without
            // block_count moving, so the page index is stale all the same.
            sink.pages_dirty = true;
        }
        return false;
    }

    let line = sink.line.clone();
    let role = sink.line_role;
    let align = sink.line_align;
    // The style the line opens in, not its first marker's: `plain {1}italic`
    // opens Regular. The drawer derives the same from the text, so caches
    // that stored the first marker here still draw right.
    let style = reader_layout::line_opening_style(line.as_str(), FontStyle::Regular);
    if sink.generate_toc_from_headings
        && !sink.generated_toc_for_spine
        && matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
    {
        push_heading_toc_record(sink, line.as_str(), role);
    }
    let mut pushed = sink.library.push_line_block(
        line.as_str(),
        style,
        role,
        align,
        paragraph_end,
        sink.spine_index,
    );
    if !pushed {
        // The section arena (text bytes or the block table) just filled.
        // Flush what we have to a section file and retry the line into a
        // fresh arena, so a long chapter chunks and continues instead of
        // losing its tail. (At the book-wide section ceiling flush_section
        // refuses and sets book_partial; the line is then genuinely
        // dropped, which is the separate whole-book limit.)
        sink.flush_section(false, true);
        pushed = sink.library.push_line_block(
            line.as_str(),
            style,
            role,
            align,
            paragraph_end,
            sink.spine_index,
        );
    }
    if sink.line_image_height > 0 {
        // Its record says it holds images, and how tall the tallest is, so
        // pagination need not read its text. (A line with an image is never
        // blank, so a push that succeeded pushed it.)
        if pushed {
            sink.library
                .mark_last_block_inline_images(sink.line_image_height);
        }
        sink.line_image_height = 0;
    }
    if sink.line_unindented {
        if pushed {
            sink.library.mark_last_block_unindented();
        }
        sink.line_unindented = false;
    }
    sink.pages_dirty = true;
    sink.hyphen_run = if sink.line_hyphenated && !paragraph_end {
        sink.hyphen_run.saturating_add(1)
    } else {
        0
    };
    sink.line_hyphenated = false;
    sink.line.clear();
    sink.reset_line_ink();
    sink.line_style = FontStyle::Regular;
    sink.pending_space = false;
    pushed
}

/// A chapter entry named after a heading line, for a book whose TOC is made
/// from its headings. Out of line: its title buffer stays off the line
/// flush's frame, which every line of every book passes through.
#[inline(never)]
fn push_heading_toc_record<
    D,
    T,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
>(
    sink: &mut LibraryBlockSink<'_, '_, D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
    line: &str,
    role: TextRole,
) where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut title = String::<160>::new();
    if sink.line_image_height > 0 {
        // A heading's images are not its title.
        for piece in inline_pieces(line) {
            if let InlinePiece::Text(run) = piece {
                push_plain_styled_text(run, &mut title);
            }
        }
        trim_leading_space(&mut title);
    } else {
        push_plain_styled_text(line, &mut title);
    }
    trim_trailing_space(&mut title);
    if !title.is_empty()
        && sink.library.push_toc_record(
            title.as_str(),
            heading_toc_level(role),
            sink.spine_index as i16,
        )
    {
        sink.generated_toc_for_spine = true;
    }
}

fn heading_toc_level(role: TextRole) -> u8 {
    match role {
        TextRole::Heading1 => 1,
        TextRole::Heading2 => 2,
        TextRole::Heading3 => 3,
        TextRole::Body | TextRole::BlockQuote => 1,
    }
}

fn push_plain_styled_text<const N: usize>(styled: &str, out: &mut String<N>) {
    let mut skip_style_code = false;
    for ch in styled.chars() {
        if skip_style_code {
            skip_style_code = false;
            continue;
        }
        if ch == reader_layout::STYLE_MARKER {
            skip_style_code = true;
            continue;
        }
        let _ = out.push(ch);
    }
}

fn is_leading_punctuation_word(word: &str) -> bool {
    word.chars()
        .next()
        .map(|ch| {
            matches!(
                ch,
                ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\u{2019}' | '\u{201D}'
            )
        })
        .unwrap_or(false)
}

fn block_align_for(run_align: TextAlign, block: &str, role: TextRole) -> TextAlign {
    if run_align == TextAlign::Center
        || matches!(
            role,
            TextRole::Heading1 | TextRole::Heading2 | TextRole::Heading3
        )
        || is_decorative_separator(block)
    {
        TextAlign::Center
    } else {
        run_align
    }
}

fn normalize_decorative_separator<const N: usize>(block: &mut String<N>) {
    if !is_decorative_separator(block.as_str()) {
        return;
    }
    block.clear();
    let _ = block.push_str("* * *");
}

fn is_decorative_separator(text: &str) -> bool {
    let mut saw_mark = false;
    let mut mark_count = 0u8;
    for ch in text.chars() {
        if ch == '*' {
            saw_mark = true;
            mark_count = mark_count.saturating_add(1);
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        return false;
    }
    saw_mark && mark_count >= 3
}

/// Collapse ASCII whitespace and drop invisible format characters, and with
/// `drop_object_marks`, U+FFFC, which starts an inline image's token.
/// Entities were already decoded by the XHTML parser; decoding again here
/// turned literal text such as `&lt;` (from `&amp;lt;`) into markup
/// characters.
fn push_normalized<const N: usize>(text: &str, out: &mut String<N>, drop_object_marks: bool) {
    let mut previous_space = true;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            if !previous_space && out.push(' ').is_err() {
                break;
            }
            previous_space = true;
        } else if ch == SOFT_HYPHEN {
            // Kept inside words as a break hint; stripped when stored.
            if out.push(ch).is_err() {
                break;
            }
        } else if is_invisible_format_char(ch) || (drop_object_marks && ch == IMAGE_TOKEN_MARK) {
            continue;
        } else if push_normalized_char(ch, out).is_err() {
            break;
        } else {
            previous_space = false;
        }
    }
}

const SOFT_HYPHEN: char = '\u{00AD}';

/// Characters with no glyph and no width: soft hyphen, zero-width
/// space/joiners, word joiner, byte-order mark. Without this they render as
/// missing-glyph boxes in the middle of words.
fn is_invisible_format_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}'
    )
}

fn push_normalized_char<const N: usize>(ch: char, out: &mut String<N>) -> Result<(), ()> {
    match ch {
        ch if ch as u32 <= u16::MAX as u32 => out.push(ch).map_err(|_| ()),
        _ => out.push('?').map_err(|_| ()),
    }
}

fn is_epub_titlepage_label(text: &str) -> bool {
    let lower = LowerAscii::<128>::new(text);
    lower.starts_with(": ")
        || lower.eq("title")
        || lower.eq("author")
        || lower.eq("creator")
        || lower.eq("language")
        || lower.eq("english")
        || lower.eq("english:")
        || lower.eq("release date")
        || lower.eq("original publication")
        || lower.starts_with("most recently updated")
        || lower.starts_with("other information")
        || lower.starts_with("other formats")
        || lower.starts_with("credits")
        || lower.starts_with("produced by")
        || lower.starts_with("transcribed from")
        || lower.starts_with("project gutenberg")
        || lower.starts_with("the project gutenberg")
}

/// Out of line: its copy of the block sits on its own frame for the check,
/// not on the line builder's for the whole of a run's placement.
#[inline(never)]
fn sanitize_preview_block<const N: usize>(block: &mut String<N>) -> bool {
    trim_trailing_space(block);
    trim_leading_space(block);
    if block.is_empty() {
        return false;
    }
    if is_epub_titlepage_label(block) || contains_gutenberg_metadata(block.as_str()) {
        return false;
    }
    if is_decorative_separator(block.as_str()) {
        normalize_decorative_separator(block);
        return true;
    }
    if let Some(rest) = decorative_prefix_rest(block.as_str()) {
        if rest.is_empty() {
            normalize_decorative_separator(block);
            return true;
        }
        if is_epub_titlepage_label(rest) || contains_gutenberg_metadata(rest) {
            return false;
        }
    }
    true
}

fn decorative_prefix_rest(text: &str) -> Option<&str> {
    let mut mark_count = 0u8;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '*' {
            mark_count = mark_count.saturating_add(1);
            end = index + ch.len_utf8();
            continue;
        }
        if ch.is_whitespace() {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if mark_count >= 3 {
        Some(text[end..].trim())
    } else {
        None
    }
}

fn contains_gutenberg_metadata(text: &str) -> bool {
    let lower = LowerAscii::<160>::new(text);
    lower.contains("most recently updated")
        || lower.contains("project gutenberg ebook")
        || lower.contains("start of the project gutenberg")
        || lower.contains("end of the project gutenberg")
        || lower.contains("other information and formats")
        || lower.contains("this ebook is for the use of anyone")
        || lower.contains("project gutenberg license")
        || lower.contains("www.gutenberg.org")
        || lower.contains("laws of the country where you are located")
}

fn trim_trailing_space<const N: usize>(text: &mut String<N>) {
    while text.as_str().as_bytes().last().copied() == Some(b' ') {
        text.pop();
    }
}

fn trim_leading_space<const N: usize>(text: &mut String<N>) {
    let trim_len = text
        .as_str()
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_whitespace())
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    if trim_len == 0 {
        return;
    }
    let mut trimmed = String::<N>::new();
    let _ = trimmed.push_str(&text.as_str()[trim_len..]);
    *text = trimmed;
}

pub(crate) struct LowerAscii<const N: usize> {
    text: String<N>,
}

impl<const N: usize> LowerAscii<N> {
    pub(crate) fn new(input: &str) -> Self {
        let mut text = String::new();
        for byte in input.bytes() {
            if text.push((byte as char).to_ascii_lowercase()).is_err() {
                break;
            }
        }
        Self { text }
    }

    pub(crate) fn eq(&self, other: &str) -> bool {
        self.text.as_str() == other
    }

    pub(crate) fn starts_with(&self, other: &str) -> bool {
        self.text.as_str().starts_with(other)
    }

    pub(crate) fn ends_with(&self, other: &str) -> bool {
        self.text.as_str().ends_with(other)
    }

    pub(crate) fn contains(&self, other: &str) -> bool {
        self.text.as_str().contains(other)
    }

    pub(crate) fn word_eq(&self, other: &str) -> bool {
        self.text
            .as_str()
            .split_ascii_whitespace()
            .any(|word| word == other)
    }
}
