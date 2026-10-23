//! The words of the last few justified lines, which the line builder can
//! take back and break again, and the search that does it.
//!
//! The builder places words a line at a time and stores each line as it
//! always has (`fw::block_sink`). It also keeps each word here as it is
//! placed, with the measure it took (`ui::reading::InkPiece`) and what joins
//! it to the word before, and a record of where each stored line starts.
//! Most lines stay as placed. When one is left ragged by the draw rule, or
//! is a third hyphenated line in a row, the builder takes back the lines
//! from a few before it once a few more follow it, and the search weighs
//! every way of breaking their words again (Knuth and Plass's total fit,
//! without fitness classes). A candidate line's width is a sum of the
//! words' measures and joins that comes out exactly as the drawer measures
//! the stored line.
//!
//! The search judges a line as the page will set it (`justify_extra`): how
//! far its gaps stretch, or how short it falls when the page leaves it
//! ragged. A word is looked up in the hyphenation patterns only where it
//! straddles the end of a line that would be loose without it. Lookups,
//! the builder's own and the search's, go through a cache of break
//! positions that lasts the spine: on the device each lookup walks a trie
//! in flash through a cold cache.
//!
//! Everything lives in borrowed scratch (the XHTML window's idle tail
//! while a spine streams), encoded by hand like `PendingImages`.

use display::font::{fixed_ceil, style_from_marker_code, style_marker_code, FontStyle};
use proto::hyphen::{WordBreak, MAX_WORD_BREAKS};
use proto::text::{TextAlign, TextRole};
use ui::reading::{justify_extra, justify_inline_extra, InkPiece, READER_WRAP_SAFETY};

/// Scratch the window borrows.
pub(crate) const LINE_WINDOW_BYTES: usize = 9 * 1024;
/// Longest word, in bytes, the window takes; a paragraph with a longer one
/// keeps the lines it was given.
pub(crate) const MAX_WINDOW_WORD: usize = 255;
/// Lines taken back before the first that needs repair, and lines stored
/// after it before the repair, so the search can move words both ways.
pub(crate) const LOOK_BACK: usize = 3;
pub(crate) const LOOK_AHEAD: usize = 2;
/// Lines a repair stored that a later one may take back again.
pub(crate) const SETTLED_KEEP: usize = 1;
/// A word straddling a candidate line's end is looked up only when the
/// line would stretch past this many half-spaces a gap without it.
pub(crate) const LOOK_UP_HALVES: i32 = 1;

const TEXT_BYTES: usize = 1024;
const MAX_WORDS: usize = 128;
const WORD_BYTES: usize = 16;
const MAX_BREAKS: usize = 96;
/// A break slot: its word, where the next line resumes (with flags), and
/// the head's measure from the word's start.
const BREAK_BYTES: usize = 9;
const NODE_BYTES: usize = 11;
/// Records of stored lines the window can take back.
const MAX_LINES: usize = 16;
const LINE_BYTES: usize = 8;
/// Cached break positions: a word's hash, which of its byte offsets a
/// line may resume at, which of those draw a hyphen, and its length.
const CACHE_SLOTS: usize = 96;
const CACHE_BYTES: usize = 25;
/// Longest word the cache keeps, so its offsets fit a 64-bit mask.
const CACHE_WORD: usize = 63;
/// Node `k` is the start of word `k` (the window's end for `k == words`);
/// a looked-up break is node `BREAK_NODES + its slot`.
const BREAK_NODES: usize = MAX_WORDS + 1;
const MAX_NODES: usize = BREAK_NODES + MAX_BREAKS;
const WORDS_AT: usize = TEXT_BYTES;
const BREAKS_AT: usize = WORDS_AT + MAX_WORDS * WORD_BYTES;
const NODES_AT: usize = BREAKS_AT + MAX_BREAKS * BREAK_BYTES;
const LINES_AT: usize = NODES_AT + MAX_NODES * NODE_BYTES;
const CACHE_AT: usize = LINES_AT + MAX_LINES * LINE_BYTES;
const _: () = assert!(CACHE_AT + CACHE_SLOTS * CACHE_BYTES <= LINE_WINDOW_BYTES);
const _: () = assert!(MAX_WINDOW_WORD <= LEN_MASK as usize && MAX_WORDS <= 256);

/// Word record flags, above the 8-bit length and 2-bit style.
const LEN_MASK: u16 = 0xff;
const STYLE_SHIFT: u16 = 8;
const DASH: u16 = 1 << 10;
const MEASURED: u16 = 1 << 11;
const SPACE: u16 = 1 << 12;
const MAY_HYPHENATE: u16 = 1 << 13;
const IMAGE: u16 = 1 << 14;
const LOOKED_UP: u16 = 1 << 15;
/// Break slot flags, above the tail's offset.
const TAIL_MASK: u16 = 0x3fff;
const SOFT: u16 = 1 << 15;
const DRAWN_HYPHEN: u16 = 1 << 14;
/// Node flags: the line ending here ends in a hyphen (drawn, or the
/// compound's own), and whether the builder drew it.
const ENDS_HYPHEN: u8 = 1;
const INSERTED: u8 = 2;
const UNREACHED: u64 = u64::MAX;
/// Line record flags.
const LINE_BAD: u8 = 1;
const LINE_UNINDENTED: u8 = 2;

/// Demerits as TeX weighs them, with its badness of 100 r^3 for a line
/// whose gaps stretch r half-spaces (6,400 at the draw rule's two spaces).
/// A line the page leaves ragged costs a premium over any it justifies,
/// more the shorter it falls, and a short one (under two thirds of the
/// measure, which reads as a paragraph's end) more again.
const LINE_PENALTY: u64 = 10;
const HYPHEN_DEMERITS: u64 = 50 * 50;
const DOUBLE_HYPHEN_DEMERITS: u64 = 300_000;
const FINAL_HYPHEN_DEMERITS: u64 = 5_000;
const SHORT_LAST_DEMERITS: u64 = 20_000;
const NO_SPACE_DEMERITS: u64 = 1_000_000;
const RAGGED_BADNESS: u32 = 12_000;
const SHORT_BADNESS: u32 = 3 * RAGGED_BADNESS;
const OVERFULL_DEMERITS: u64 = 100_000_000;
const MAX_BADNESS: u64 = 1_000_000_000;

/// What the window needs measured and looked up, the builder's way.
pub(crate) trait Measure {
    /// `text` alone (soft hyphens skipped) in `style`, then a drawn hyphen
    /// when `hyphen`.
    fn piece(&mut self, text: &str, style: FontStyle, hyphen: bool) -> InkPiece;
    /// What joins a piece opening with glyph `first` in `style` to the one
    /// before it on a line, which ended with glyph `previous.1` in style
    /// `previous.0` (None after an image): a space when `space`, and kerning.
    fn join(
        &mut self,
        previous: (FontStyle, Option<u16>),
        space: bool,
        style: FontStyle,
        first: Option<u16>,
    ) -> i32;
    /// The normal word space the page justifies a line opening in `style`
    /// with.
    fn space(&mut self, style: FontStyle) -> i16;
    /// Where `word` may break at a line end, from the patterns.
    fn breaks(&mut self, word: &str, out: &mut heapless::Vec<WordBreak, MAX_WORD_BREAKS>);
    /// The cache answered a lookup.
    fn hit(&mut self) {}
    /// The search weighed a candidate line.
    fn weigh(&mut self) {}
}

/// How a search ends.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum End {
    /// More words follow: the last line is only where these run out, and
    /// stays in hand for them.
    Window,
    /// Its words end their lines here, for good; `paragraph` when they end
    /// a paragraph too.
    Final { paragraph: bool },
}

/// One word (or inline image) of the window, from its record.
#[derive(Clone, Copy)]
pub(crate) struct Word {
    offset: usize,
    len: usize,
    pub(crate) style: FontStyle,
    /// The source has a space before it (dropped at a line's start).
    pub(crate) space: bool,
    pub(crate) may_hyphenate: bool,
    pub(crate) image: bool,
    looked_up: bool,
    /// Ends in an en or em dash.
    dash: bool,
    measured: bool,
    /// Its last glyph, which kerns across the join to the word after.
    last: Option<u16>,
    advance_fp: i32,
    ink_at_fp: i32,
    ink: i16,
    join_fp: i32,
    /// An image's height, or a looked-up word's first break slot and count.
    extra: u16,
}

impl Word {
    fn piece(&self) -> InkPiece {
        InkPiece::from_parts(self.advance_fp, self.ink_at_fp, self.ink)
    }

    /// An image's height; 0 for a word.
    pub(crate) fn image_height(&self) -> u16 {
        if self.image {
            self.extra
        } else {
            0
        }
    }

    /// The slots of its breaks, once looked up.
    fn breaks(&self) -> core::ops::Range<usize> {
        if !self.looked_up || self.image {
            return 0..0;
        }
        let first = usize::from(self.extra & 0xff);
        first..first + usize::from(self.extra >> 8)
    }
}

/// Where a line to store starts and ends in the window: from byte `from`
/// of word `first` to word `last`, whole, or to byte `head_end` of it with
/// a drawn hyphen when `hyphen`.
#[derive(Clone, Copy)]
pub(crate) struct Span {
    pub(crate) first: usize,
    pub(crate) from: usize,
    pub(crate) last: usize,
    pub(crate) head: Option<(usize, bool)>,
}

/// A stored line the window can take back: its block, and the builder's
/// state before it.
#[derive(Clone, Copy)]
pub(crate) struct LineRecord {
    pub(crate) block: usize,
    /// Where its words start: a word, and a byte in it.
    start: (usize, usize),
    /// Hyphenated lines in a row before it.
    pub(crate) run_before: u8,
    /// It wants repair, and has not had it.
    bad: bool,
    /// A drop cap opens it.
    pub(crate) unindented: bool,
}

/// A candidate line, as the search judges it.
#[derive(Clone, Copy)]
struct Candidate {
    width: i16,
    gaps: usize,
    opening: FontStyle,
    height: u16,
    x: i16,
    flags: u8,
    /// Ends after an en or em dash.
    dash: bool,
    /// Ends where the source has no space.
    no_space: bool,
    overfull: bool,
}

pub(crate) struct LineWindow<'a> {
    buf: &'a mut [u8],
    words: usize,
    text_len: usize,
    breaks: usize,
    lines: usize,
    /// The first line record that wants repair.
    bad_line: Option<usize>,
    /// The window's first line starts this far into word 0.
    root_offset: usize,
    /// The line stored before the window's first ended in a hyphen.
    root_hyphen: bool,
    /// It keeps the words placed, of `role` text set at `align`, from `x`
    /// to `right`, a paragraph's first line indented by `indent`.
    active: bool,
    pub(crate) role: TextRole,
    pub(crate) align: TextAlign,
    x: i16,
    right: i16,
    indent: i16,
    /// Where the line in hand starts, and the piece being placed (where
    /// the next line starts, once the line in hand is stored).
    hand_start: (usize, usize),
    piece_at: (usize, usize),
    /// What the next word joins to (the style and last glyph before it),
    /// and what the word being placed joins to.
    joins: Option<(FontStyle, Option<u16>)>,
    piece_joins: Option<(FontStyle, Option<u16>)>,
    /// Justification spaces by style, once measured.
    spaces: [i16; 4],
}

impl<'a> LineWindow<'a> {
    pub(crate) fn new(buf: &'a mut [u8]) -> Self {
        let window = Self {
            buf,
            words: 0,
            text_len: 0,
            breaks: 0,
            lines: 0,
            bad_line: None,
            root_offset: 0,
            root_hyphen: false,
            active: false,
            role: TextRole::Body,
            align: TextAlign::Justify,
            x: 0,
            right: 0,
            indent: 0,
            hand_start: (0, 0),
            piece_at: (0, 0),
            joins: None,
            piece_joins: None,
            spaces: [0; 4],
        };
        // Whatever the scratch held is no cache.
        if window.usable() {
            window.buf[CACHE_AT..CACHE_AT + CACHE_SLOTS * CACHE_BYTES].fill(0);
        }
        window
    }

    /// Whether it was lent room enough to work in.
    pub(crate) fn usable(&self) -> bool {
        self.buf.len() >= LINE_WINDOW_BYTES
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// Whether it keeps the words of `role` text set at `align`.
    pub(crate) fn keeps(&self, role: TextRole, align: TextAlign) -> bool {
        self.active && self.role == role && self.align == align
    }

    /// Forgets every word and line, and keeps no more until it begins
    /// again: a paragraph ended, or the builder stored what the window
    /// cannot take back.
    pub(crate) fn clear(&mut self) {
        self.active = false;
        self.words = 0;
        self.text_len = 0;
        self.breaks = 0;
        self.lines = 0;
        self.bad_line = None;
        self.root_offset = 0;
        self.root_hyphen = false;
        self.hand_start = (0, 0);
        self.piece_at = (0, 0);
        self.joins = None;
        self.piece_joins = None;
    }

    /// Starts keeping the words placed on an empty line in hand, of `role`
    /// text set at `align` from `x` to `right`, whose paragraphs indent
    /// their first line by `indent`.
    pub(crate) fn begin(
        &mut self,
        role: TextRole,
        align: TextAlign,
        x: i16,
        right: i16,
        indent: i16,
    ) {
        self.clear();
        self.active = self.usable();
        self.role = role;
        self.align = align;
        self.x = x;
        self.right = right;
        self.indent = indent;
    }

    pub(crate) fn indent(&self) -> i16 {
        self.indent
    }

    /// The window's left edge and right margin.
    pub(crate) fn edges(&self) -> (i16, i16) {
        (self.x, self.right)
    }

    /// Keeps a word about to be placed; false when there is no room.
    pub(crate) fn push_word(
        &mut self,
        text: &str,
        style: FontStyle,
        space: bool,
        may_hyphenate: bool,
    ) -> bool {
        let flags = style_bits(style)
            | if space { SPACE } else { 0 }
            | if may_hyphenate { MAY_HYPHENATE } else { 0 }
            | if ends_with_dash(text) { DASH } else { 0 };
        if !self.push(text, flags, 0, 0) {
            return false;
        }
        self.piece_joins = self.joins;
        self.joins = Some((style, None));
        true
    }

    /// Keeps an inline image about to be placed: its token and fitted size.
    pub(crate) fn push_image(&mut self, token: &str, width: u16, height: u16, space: bool) -> bool {
        let flags = IMAGE | if space { SPACE } else { 0 };
        if !self.push(token, flags, i32::from(width) << 4, height) {
            return false;
        }
        self.piece_joins = self.joins;
        self.joins = Some((
            self.joins.map_or(FontStyle::Regular, |(style, _)| style),
            None,
        ));
        true
    }

    /// What the word being placed joins to, when it is a word.
    pub(crate) fn piece_joins(&self) -> Option<(FontStyle, Option<u16>)> {
        self.piece_joins
    }

    /// The measure of the word being placed, `text` from its start, once
    /// the builder measured it (a piece that moves to the next line is
    /// placed again).
    pub(crate) fn current_piece(&self, text: &str) -> Option<InkPiece> {
        let word = self.current_word(text)?;
        if !word.measured {
            return None;
        }
        let mut piece = word.piece();
        piece.last = word.last;
        Some(piece)
    }

    /// Keeps the builder's measure of the word being placed, `text` from
    /// its start, with what joins it to the word before: `join_fp` when
    /// the builder placed it after that word on its line, else measured.
    pub(crate) fn keep_measure(
        &mut self,
        text: &str,
        piece: &InkPiece,
        join_fp: Option<i32>,
        measure: &mut impl Measure,
    ) {
        let Some(mut word) = self.current_word(text) else {
            return;
        };
        if word.measured {
            return;
        }
        (word.advance_fp, word.ink_at_fp, word.ink) = piece.parts();
        word.last = piece.last;
        word.join_fp = match (join_fp, self.piece_joins) {
            (Some(join_fp), _) => join_fp,
            (None, Some(previous)) => measure.join(previous, word.space, word.style, piece.first),
            (None, None) => 0,
        };
        let index = self.piece_at.0;
        let packed = self.at(WORDS_AT + index * WORD_BYTES + 2, 2) as u16 | MEASURED;
        self.write_word(index, &word, packed);
        self.joins = Some((word.style, piece.last));
    }

    /// The word being placed, when `text` is all of it.
    fn current_word(&self, text: &str) -> Option<Word> {
        let (index, offset) = self.piece_at;
        if !self.active || offset != 0 || index >= self.words {
            return None;
        }
        let word = self.word(index);
        (!word.image && self.text(&word) == text).then_some(word)
    }

    fn push(&mut self, text: &str, flags: u16, advance_fp: i32, extra: u16) -> bool {
        if !self.active
            || self.words >= MAX_WORDS
            || text.len() > MAX_WINDOW_WORD
            || self.text_len + text.len() > TEXT_BYTES
        {
            return false;
        }
        let offset = self.text_len;
        self.buf[offset..offset + text.len()].copy_from_slice(text.as_bytes());
        self.text_len += text.len();
        let image = flags & IMAGE != 0;
        let word = Word {
            offset,
            len: text.len(),
            style: FontStyle::Regular,
            space: false,
            may_hyphenate: false,
            image,
            looked_up: false,
            dash: false,
            measured: false,
            last: None,
            advance_fp,
            ink_at_fp: 0,
            ink: (advance_fp >> 4) as i16,
            join_fp: 0,
            extra,
        };
        self.write_word(self.words, &word, text.len() as u16 | flags);
        self.piece_at = (self.words, 0);
        self.words += 1;
        true
    }

    /// The piece being placed now resumes `by` bytes further into its word:
    /// a head of it ends the line about to be stored.
    pub(crate) fn advance_piece(&mut self, by: usize) {
        self.piece_at.1 += by;
    }

    /// Measures every word not yet measured, with what joins it to the word
    /// before.
    #[inline(never)]
    pub(crate) fn measure(&mut self, measure: &mut impl Measure) {
        let mut before: Option<(FontStyle, Option<u16>)> = None;
        for index in 0..self.words {
            let mut word = self.word(index);
            if !word.measured {
                let mut first = None;
                if !word.image {
                    let piece = measure.piece(self.text(&word), word.style, false);
                    (word.advance_fp, word.ink_at_fp, word.ink) = piece.parts();
                    word.last = piece.last;
                    first = piece.first;
                }
                word.join_fp = match before {
                    // An image joins as a space in the style before it.
                    Some(previous) if word.image => {
                        measure.join(previous, word.space, previous.0, None)
                    }
                    Some(previous) => measure.join(previous, word.space, word.style, first),
                    None => 0,
                };
                let packed = self.at(WORDS_AT + index * WORD_BYTES + 2, 2) as u16 | MEASURED;
                self.write_word(index, &word, packed);
                word.measured = true;
            }
            before = Some(match word.image {
                true => (before.map_or(FontStyle::Regular, |(style, _)| style), None),
                false => (word.style, word.last),
            });
        }
        if before.is_some() {
            self.joins = before;
        }
    }

    // Lines stored, and taking them back.

    /// Notes the line in hand as stored, as block `block`: the next line
    /// starts at the piece being placed.
    pub(crate) fn note_line(&mut self, block: usize, run_before: u8, bad: bool, unindented: bool) {
        if !self.active {
            return;
        }
        if self.lines == MAX_LINES {
            self.forget_first(1);
        }
        let record = LineRecord {
            block,
            start: self.hand_start,
            run_before,
            bad,
            unindented,
        };
        self.write_line(self.lines, record);
        if bad && self.bad_line.is_none() {
            self.bad_line = Some(self.lines);
        }
        self.lines += 1;
        self.hand_start = self.piece_at;
    }

    /// The line in hand starts at `start` (after a repair).
    pub(crate) fn set_hand_start(&mut self, start: (usize, usize)) {
        self.hand_start = start;
    }

    /// Lets go of the lines no repair can take back: all but `LOOK_BACK`
    /// before the first that wants repair, or before the line in hand.
    /// Their words go once they fill half the window (moving the rest is
    /// not worth doing a line at a time).
    pub(crate) fn trim(&mut self) {
        if !self.active {
            return;
        }
        let keep_from = self.first_bad().unwrap_or(self.lines);
        self.forget_first(keep_from.saturating_sub(LOOK_BACK));
        if self.words > MAX_WORDS / 2 || self.text_len > TEXT_BYTES / 2 {
            let start = match self.lines {
                0 => self.hand_start,
                _ => self.line(0).start,
            };
            self.drop_words(start);
        }
    }

    /// Lets go of the lines a repair just stored, but for the last
    /// `SETTLED_KEEP`: the search weighed them already, and a later repair
    /// may take back only those.
    pub(crate) fn settle(&mut self) {
        self.forget_first(self.lines.saturating_sub(SETTLED_KEEP));
        self.trim();
    }

    /// A stored line wants repair and enough lines follow it to move words
    /// into.
    pub(crate) fn repair_due(&self) -> bool {
        self.first_bad()
            .is_some_and(|bad| self.lines - bad > LOOK_AHEAD)
    }

    pub(crate) fn repair_pending(&self) -> bool {
        self.first_bad().is_some()
    }

    fn first_bad(&self) -> Option<usize> {
        self.bad_line
    }

    /// Takes back the lines from `LOOK_BACK` before the first that wants
    /// repair: returns the first of them, whose block on the builder
    /// truncates its store to. The window's words are then theirs and the
    /// line in hand's, from that line's start.
    pub(crate) fn take_back(&mut self) -> Option<LineRecord> {
        let bad = self.first_bad()?;
        let root = self.line(bad.saturating_sub(LOOK_BACK));
        self.lines = 0;
        self.bad_line = None;
        self.drop_words(root.start);
        self.root_hyphen = root.run_before > 0;
        Some(root)
    }

    /// Lines stored in a section already written can no longer be taken
    /// back.
    pub(crate) fn forget_lines(&mut self) {
        self.lines = 0;
        self.bad_line = None;
    }

    /// Forgets the first `count` records (their words stay until `trim`).
    fn forget_first(&mut self, count: usize) {
        let count = count.min(self.lines);
        if count == 0 {
            return;
        }
        let from = LINES_AT + count * LINE_BYTES;
        let to = LINES_AT + self.lines * LINE_BYTES;
        self.buf.copy_within(from..to, LINES_AT);
        self.lines -= count;
        self.bad_line = match self.bad_line {
            Some(bad) if bad >= count => Some(bad - count),
            _ => (0..self.lines).find(|&index| self.line(index).bad),
        };
    }

    fn line(&self, index: usize) -> LineRecord {
        let at = LINES_AT + index * LINE_BYTES;
        let flags = self.buf[at + 7];
        LineRecord {
            block: self.at(at, 2) as usize,
            start: (usize::from(self.buf[at + 2]), self.at(at + 3, 2) as usize),
            run_before: self.buf[at + 6],
            bad: flags & LINE_BAD != 0,
            unindented: flags & LINE_UNINDENTED != 0,
        }
    }

    fn write_line(&mut self, index: usize, record: LineRecord) {
        let at = LINES_AT + index * LINE_BYTES;
        self.put(at, 2, record.block as u32);
        self.buf[at + 2] = record.start.0 as u8;
        self.put(at + 3, 2, record.start.1 as u32);
        self.buf[at + 5] = 0;
        self.buf[at + 6] = record.run_before;
        self.buf[at + 7] = if record.bad { LINE_BAD } else { 0 }
            | if record.unindented {
                LINE_UNINDENTED
            } else {
                0
            };
    }

    /// Drops the words before position `start`, where the window's first
    /// line then starts; positions, records and break slots move with them.
    fn drop_words(&mut self, start: (usize, usize)) {
        let first = start.0.min(self.words);
        self.root_offset = if first < self.words { start.1 } else { 0 };
        if first == 0 {
            return;
        }
        let base = if first < self.words {
            self.word(first).offset
        } else {
            self.text_len
        };
        self.buf.copy_within(base..self.text_len, 0);
        self.text_len -= base;
        for index in first..self.words {
            let mut word = self.word(index);
            let packed = self.at(WORDS_AT + index * WORD_BYTES + 2, 2) as u16;
            word.offset -= base;
            self.write_word(index - first, &word, packed);
        }
        self.words -= first;
        // The remaining words' breaks move to the front in slot order, which
        // only ever moves a slot down (a search may look up an earlier word
        // after a later one); each word learns its first slot again.
        let mut kept = 0usize;
        for slot in 0..self.breaks {
            let at = BREAKS_AT + slot * BREAK_BYTES;
            let owner = usize::from(self.buf[at]);
            if owner < first {
                continue;
            }
            let owner = owner - first;
            let to = BREAKS_AT + kept * BREAK_BYTES;
            self.buf.copy_within(at..at + BREAK_BYTES, to);
            self.buf[to] = owner as u8;
            let word = self.word(owner);
            if word.breaks().start == slot {
                let count = word.breaks().len();
                let moved = Word {
                    extra: (kept | count << 8) as u16,
                    ..word
                };
                let packed = self.at(WORDS_AT + owner * WORD_BYTES + 2, 2) as u16;
                self.write_word(owner, &moved, packed);
            }
            kept += 1;
        }
        self.breaks = kept;
        let shift = |(word, offset): (usize, usize)| (word.saturating_sub(first), offset);
        self.hand_start = shift(self.hand_start);
        self.piece_at = shift(self.piece_at);
        for index in 0..self.lines {
            let mut record = self.line(index);
            record.start = shift(record.start);
            self.write_line(index, record);
        }
    }

    // Break positions.

    /// Where `word` may break, from the patterns or the cache: a word of up
    /// to `CACHE_WORD` bytes without soft hyphens (whose breaks drop a byte
    /// the cache does not keep) is looked up once per spine.
    pub(crate) fn cached_breaks(
        &mut self,
        word: &str,
        measure: &mut impl Measure,
        out: &mut heapless::Vec<WordBreak, MAX_WORD_BREAKS>,
    ) {
        out.clear();
        if !self.usable() || word.len() > CACHE_WORD || word.contains('\u{ad}') {
            measure.breaks(word, out);
            return;
        }
        let hash = word_hash(word);
        let at = CACHE_AT + (hash % CACHE_SLOTS as u64) as usize * CACHE_BYTES;
        if self.mask(at) == hash && usize::from(self.buf[at + 24]) == word.len() {
            let (tails, drawn) = (self.mask(at + 8), self.mask(at + 16));
            for end in 1..word.len() {
                if tails & 1 << end != 0 {
                    let _ = out.push(WordBreak {
                        head_end: end,
                        tail_start: end,
                        hyphen: drawn & 1 << end != 0,
                    });
                }
            }
            measure.hit();
            return;
        }
        measure.breaks(word, out);
        let (mut tails, mut drawn) = (0u64, 0u64);
        for item in out.iter() {
            tails |= 1 << item.tail_start;
            if item.hyphen {
                drawn |= 1 << item.tail_start;
            }
        }
        self.buf[at..at + 8].copy_from_slice(&hash.to_le_bytes());
        self.buf[at + 8..at + 16].copy_from_slice(&tails.to_le_bytes());
        self.buf[at + 16..at + 24].copy_from_slice(&drawn.to_le_bytes());
        self.buf[at + 24] = word.len() as u8;
    }

    fn mask(&self, at: usize) -> u64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.buf[at..at + 8]);
        u64::from_le_bytes(bytes)
    }

    /// Looks word `index`'s breaks up, once while it is kept, and measures
    /// the head each leaves.
    #[inline(never)]
    fn look_up(&mut self, index: usize, measure: &mut impl Measure) {
        let word = self.word(index);
        if word.looked_up {
            return;
        }
        let mut found = heapless::Vec::<WordBreak, MAX_WORD_BREAKS>::new();
        if word.len <= CACHE_WORD {
            let mut copy = [0u8; CACHE_WORD];
            copy[..word.len].copy_from_slice(&self.buf[word.offset..word.offset + word.len]);
            let text = core::str::from_utf8(&copy[..word.len]).unwrap_or("");
            self.cached_breaks(text, measure, &mut found);
        } else {
            measure.breaks(self.text(&word), &mut found);
        }
        let first = self.breaks;
        let count = found.len().min(MAX_BREAKS - first);
        for (slot, item) in found.iter().take(count).enumerate() {
            let soft = item.head_end != item.tail_start;
            let packed = (item.tail_start as u16 & TAIL_MASK)
                | if soft { SOFT } else { 0 }
                | if item.hyphen { DRAWN_HYPHEN } else { 0 };
            let head = measure.piece(&self.text(&word)[..item.head_end], word.style, item.hyphen);
            let (advance_fp, ink_at_fp, ink) = head.parts();
            let at = BREAKS_AT + (first + slot) * BREAK_BYTES;
            self.buf[at] = index as u8;
            self.put(at + 1, 2, u32::from(packed));
            self.put(at + 3, 2, advance_fp.clamp(0, 0xffff) as u32);
            self.put(at + 5, 2, ink_at_fp.clamp(0, 0xffff) as u32);
            self.put(at + 7, 2, u32::from(ink as u16));
            // A slot new to this search starts unreached, like every node.
            self.set_node(BREAK_NODES + first + slot, UNREACHED, 0, 0);
        }
        self.breaks += count;
        let looked_up = Word {
            looked_up: true,
            extra: (first | count << 8) as u16,
            ..word
        };
        let at = WORDS_AT + index * WORD_BYTES;
        let packed = self.at(at + 2, 2) as u16 | LOOKED_UP;
        self.write_word(index, &looked_up, packed);
    }

    /// A looked-up break: the word it is in, where the next line resumes,
    /// where this one's head ends, and whether a hyphen is drawn.
    fn break_at(&self, slot: usize) -> (usize, usize, usize, bool) {
        let at = BREAKS_AT + slot * BREAK_BYTES;
        let packed = self.at(at + 1, 2) as u16;
        let tail = usize::from(packed & TAIL_MASK);
        let head_end = if packed & SOFT != 0 {
            tail - '\u{ad}'.len_utf8()
        } else {
            tail
        };
        (
            usize::from(self.buf[at]),
            tail,
            head_end,
            packed & DRAWN_HYPHEN != 0,
        )
    }

    /// The head a break leaves on its line, measured from its word's start.
    fn break_head(&self, slot: usize) -> InkPiece {
        let at = BREAKS_AT + slot * BREAK_BYTES;
        InkPiece::from_parts(
            self.at(at + 3, 2) as i32,
            self.at(at + 5, 2) as i32,
            self.at(at + 7, 2) as u16 as i16,
        )
    }

    // Records.

    fn at(&self, at: usize, bytes: usize) -> u32 {
        self.buf[at..at + bytes]
            .iter()
            .rev()
            .fold(0, |value, byte| value << 8 | u32::from(*byte))
    }

    fn put(&mut self, at: usize, bytes: usize, value: u32) {
        for index in 0..bytes {
            self.buf[at + index] = (value >> (8 * index)) as u8;
        }
    }

    /// Writes `word`'s record at `index`, with its length and flags packed
    /// as `packed`.
    fn write_word(&mut self, index: usize, word: &Word, packed: u16) {
        let at = WORDS_AT + index * WORD_BYTES;
        self.put(at, 2, word.offset as u32);
        self.put(at + 2, 2, u32::from(packed));
        self.put(at + 4, 2, word.advance_fp.clamp(0, 0xffff) as u32);
        self.put(at + 6, 2, u32::from(word.join_fp as i16 as u16));
        self.put(at + 8, 2, word.ink_at_fp.clamp(0, 0xffff) as u32);
        self.put(at + 10, 2, u32::from(word.ink as u16));
        self.put(at + 12, 2, u32::from(word.extra));
        self.put(at + 14, 2, u32::from(word.last.unwrap_or(u16::MAX)));
    }

    pub(crate) fn word(&self, index: usize) -> Word {
        let at = WORDS_AT + index * WORD_BYTES;
        let record: &[u8; WORD_BYTES] = self.buf[at..at + WORD_BYTES]
            .try_into()
            .unwrap_or(&[0; WORD_BYTES]);
        let field = |at: usize| u16::from_le_bytes([record[at], record[at + 1]]);
        let packed = field(2);
        Word {
            offset: usize::from(field(0)),
            len: usize::from(packed & LEN_MASK),
            style: style_from_bits(packed),
            space: packed & SPACE != 0,
            may_hyphenate: packed & MAY_HYPHENATE != 0,
            image: packed & IMAGE != 0,
            looked_up: packed & LOOKED_UP != 0,
            dash: packed & DASH != 0,
            measured: packed & MEASURED != 0,
            last: (field(14) != u16::MAX).then_some(field(14)),
            advance_fp: i32::from(field(4)),
            join_fp: i32::from(field(6) as i16),
            ink_at_fp: i32::from(field(8)),
            ink: field(10) as i16,
            extra: field(12),
        }
    }

    /// A word's text, soft hyphens included (an image's token).
    pub(crate) fn text(&self, word: &Word) -> &str {
        core::str::from_utf8(&self.buf[word.offset..word.offset + word.len]).unwrap_or("")
    }

    fn node(&self, node: usize) -> (u64, usize, u8) {
        let at = NODES_AT + node * NODE_BYTES;
        let record: &[u8; NODE_BYTES] = self.buf[at..at + NODE_BYTES]
            .try_into()
            .unwrap_or(&[0; NODE_BYTES]);
        let mut cost = [0u8; 8];
        cost.copy_from_slice(&record[..8]);
        (
            u64::from_le_bytes(cost),
            usize::from(u16::from_le_bytes([record[8], record[9]])),
            record[10],
        )
    }

    fn cost(&self, node: usize) -> u64 {
        self.mask(NODES_AT + node * NODE_BYTES)
    }

    fn set_node(&mut self, node: usize, cost: u64, link: usize, flags: u8) {
        let at = NODES_AT + node * NODE_BYTES;
        let record = &mut self.buf[at..at + NODE_BYTES];
        record[..8].copy_from_slice(&cost.to_le_bytes());
        record[8..10].copy_from_slice(&(link as u16).to_le_bytes());
        record[10] = flags;
    }

    /// Where node `node` starts a line: its word, and the byte in it.
    fn position(&self, node: usize) -> (usize, usize) {
        if node < BREAK_NODES {
            (node, if node == 0 { self.root_offset } else { 0 })
        } else {
            let (word, tail, ..) = self.break_at(node - BREAK_NODES);
            (word, tail)
        }
    }

    /// The justification space for a line opening in `style`.
    pub(crate) fn space(&mut self, measure: &mut impl Measure, style: FontStyle) -> i16 {
        let index = usize::from(style_bits(style) >> STYLE_SHIFT);
        if self.spaces[index] == 0 {
            self.spaces[index] = measure.space(style).max(1);
        }
        self.spaces[index]
    }

    // The search.

    /// Breaks the window's words into lines; returns how many lines the
    /// best way makes, ready for `line_from`. `first_indent` is what its
    /// first line is indented by (0 unless it opens a paragraph).
    #[inline(never)]
    pub(crate) fn search(
        &mut self,
        measure: &mut impl Measure,
        first_indent: i16,
        end: End,
    ) -> usize {
        self.run(measure, first_indent, end);
        // The best path's back links, turned into forward ones.
        let (mut at, mut next, mut lines) = (self.words, self.words, 0);
        while at != 0 {
            let (cost, previous, flags) = self.node(at);
            self.set_node(at, cost, next, flags);
            next = at;
            at = previous;
            lines += 1;
        }
        let (cost, _, flags) = self.node(0);
        self.set_node(0, cost, next, flags);
        lines
    }

    /// Weighs every way of breaking the window, hyphenating where a line
    /// would otherwise be loose. Some way always reaches the end: a word
    /// too wide for a line overflows one.
    fn run(&mut self, measure: &mut impl Measure, first_indent: i16, end: End) {
        for node in (0..=self.words).chain(BREAK_NODES..BREAK_NODES + self.breaks) {
            self.set_node(node, UNREACHED, 0, 0);
        }
        let root_flags = if self.root_hyphen { ENDS_HYPHEN } else { 0 };
        self.set_node(0, 0, 0, root_flags);
        for index in 0..self.words {
            self.relax_from(index, measure, first_indent, end);
            // Then the breaks inside the word, in order: every line that
            // could end at one has looked it up by now.
            for slot in self.word(index).breaks() {
                if index > 0 || self.break_at(slot).1 > self.root_offset {
                    self.relax_from(BREAK_NODES + slot, measure, first_indent, end);
                }
            }
        }
    }

    /// Relaxes every line that can start at node `start`.
    fn relax_from(
        &mut self,
        start: usize,
        measure: &mut impl Measure,
        first_indent: i16,
        end: End,
    ) {
        let (start_cost, _, start_flags) = self.node(start);
        if start_cost == UNREACHED {
            return;
        }
        let (first, offset) = self.position(start);
        let x = self.x + if start == 0 { first_indent } else { 0 };
        let limit = self.right - READER_WRAP_SAFETY - x;
        let mut word = self.word(first);
        let opening = if word.image {
            FontStyle::Regular
        } else {
            word.style
        };
        let trigger = i32::from(self.space(measure, FontStyle::Regular)) * LOOK_UP_HALVES;
        let (mut total, mut right, mut gaps, mut height, mut width) =
            (0i32, 0i16, 0usize, 0u16, 0i16);
        // A line ending here would fall short of two thirds of the measure:
        // weighed only when no longer line from this start fits, so some
        // way always reaches the end.
        let mut short: Option<(usize, Candidate)> = None;
        for index in first..self.words {
            // The word after, for whether a line may end before it.
            let next = (index + 1 < self.words).then(|| self.word(index + 1));
            let from = if index == first { offset } else { 0 };
            let piece = if from == 0 {
                word.piece()
            } else {
                measure.piece(&self.text(&word)[from..], word.style, false)
            };
            let join = if index > first { word.join_fp } else { 0 };
            let full_total = total + join + piece.advance_fp;
            let full_right = right.max(piece.right(full_total));
            let full_width = full_right.max(fixed_ceil(full_total));
            let full_gaps = gaps + usize::from(index > first && word.space);
            let full_height = height.max(word.image_height());
            if full_width > limit {
                // Loose without the word, with room for a head of about
                // two of its letters and a hyphen: worth a look in the
                // patterns.
                let loose = index == first
                    || gaps == 0
                    || i32::from(limit - width) * 2 > gaps as i32 * trigger;
                let letters = self.text(&word)[from..].chars().count().max(1) as i32;
                let room = i32::from(limit) * 16 - (total + join);
                let roomy = room * letters >= 3 * piece.advance_fp;
                let mut split = false;
                if word.may_hyphenate && loose && roomy {
                    self.look_up(index, measure);
                    for slot in self.word(index).breaks() {
                        let (_, _, head_end, drawn) = self.break_at(slot);
                        if head_end <= from {
                            continue;
                        }
                        let text = self.text(&word);
                        let head_text = &text[from..head_end];
                        if head_text.chars().filter(|ch| ch.is_alphanumeric()).count() < 2 {
                            continue;
                        }
                        let head = if from == 0 {
                            self.break_head(slot)
                        } else {
                            measure.piece(head_text, word.style, drawn)
                        };
                        let head_total = total + join + head.advance_fp;
                        let head_width = right
                            .max(head.right(head_total))
                            .max(fixed_ceil(head_total));
                        if head_width > limit {
                            continue;
                        }
                        split = true;
                        let head_text = &self.text(&word)[..head_end];
                        let ends_hyphen = drawn || head_text.ends_with('-');
                        let line = Candidate {
                            width: head_width,
                            gaps: full_gaps,
                            opening,
                            height: full_height,
                            x,
                            flags: if ends_hyphen { ENDS_HYPHEN } else { 0 }
                                | if drawn { INSERTED } else { 0 },
                            dash: !drawn && ends_with_dash(head_text),
                            no_space: false,
                            overfull: false,
                        };
                        let target = BREAK_NODES + slot;
                        self.relax(start, start_cost, start_flags, target, line, measure, end);
                    }
                }
                if let Some((target, line)) = short {
                    self.relax(start, start_cost, start_flags, target, line, measure, end);
                }
                if index == first && !split {
                    // Wider than any line and unbreakable: it overflows.
                    let line = Candidate {
                        width: full_width,
                        gaps: full_gaps,
                        opening,
                        height: full_height,
                        x,
                        flags: 0,
                        dash: false,
                        no_space: false,
                        overfull: true,
                    };
                    self.relax(
                        start,
                        start_cost,
                        start_flags,
                        index + 1,
                        line,
                        measure,
                        end,
                    );
                }
                return;
            }
            total = full_total;
            right = full_right;
            gaps = full_gaps;
            height = full_height;
            width = full_width;
            let line = Candidate {
                width,
                gaps,
                opening,
                height,
                x,
                flags: 0,
                dash: word.dash,
                no_space: next.is_some_and(|next| !next.space),
                overfull: false,
            };
            match next {
                Some(next) if i32::from(limit - width) * 3 > i32::from(limit) => {
                    short = Some((index + 1, line));
                    word = next;
                }
                Some(next) => {
                    short = None;
                    self.relax(
                        start,
                        start_cost,
                        start_flags,
                        index + 1,
                        line,
                        measure,
                        end,
                    );
                    word = next;
                }
                None => {
                    self.relax(
                        start,
                        start_cost,
                        start_flags,
                        index + 1,
                        line,
                        measure,
                        end,
                    );
                    return;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn relax(
        &mut self,
        start: usize,
        start_cost: u64,
        start_flags: u8,
        target: usize,
        line: Candidate,
        measure: &mut impl Measure,
        end: End,
    ) {
        measure.weigh();
        // Demerits only add: a start already dearer than the target's best
        // way in cannot improve it.
        if start_cost >= self.cost(target) {
            return;
        }
        let last = target == self.words;
        let mut demerits = if last {
            // The last line is free, but a paragraph's should be more than
            // a stub, and not follow a hyphen.
            let mut demerits = LINE_PENALTY * LINE_PENALTY;
            if end == (End::Final { paragraph: true }) {
                let limit = self.right - READER_WRAP_SAFETY - line.x;
                if i32::from(line.width) * 100 < i32::from(limit) * 12 {
                    demerits += SHORT_LAST_DEMERITS;
                }
                if start_flags & ENDS_HYPHEN != 0 {
                    demerits += FINAL_HYPHEN_DEMERITS;
                }
            }
            demerits
        } else if line.overfull {
            (LINE_PENALTY + 10_000).pow(2)
        } else {
            let Some(badness) = self.badness(&line, measure) else {
                return;
            };
            let mut demerits = (LINE_PENALTY + badness).pow(2);
            if line.flags & ENDS_HYPHEN != 0 {
                demerits += HYPHEN_DEMERITS;
                if start_flags & ENDS_HYPHEN != 0 {
                    demerits += DOUBLE_HYPHEN_DEMERITS;
                }
            } else if line.dash {
                demerits += HYPHEN_DEMERITS;
            }
            if line.no_space {
                demerits += NO_SPACE_DEMERITS;
            }
            demerits
        };
        if line.overfull {
            demerits += OVERFULL_DEMERITS;
        }
        let cost = start_cost.saturating_add(demerits);
        if cost < self.cost(target) {
            let flags = if last { 0 } else { line.flags };
            self.set_node(target, cost, start, flags);
        }
    }

    /// The badness of a line the window does not end, as the page sets
    /// it; None when it does not fit.
    fn badness(&mut self, line: &Candidate, measure: &mut impl Measure) -> Option<u64> {
        let space = self.space(measure, line.opening);
        let extra = self.right - READER_WRAP_SAFETY - (line.x + line.width);
        // 100 r^3, r being the stretch in half-spaces, in 8.8 fixed point:
        // one 32-bit division, no 64-bit one (a soft routine on the C3).
        let stretched = |extra: i16, gaps: usize| {
            let room = gaps.max(1) as u32 * space as u32;
            let ratio = u64::from(((extra.max(0) as u32) << 9) / room).min(0xffff);
            ((100 * ratio * ratio * ratio) >> 24).min(MAX_BADNESS)
        };
        let ragged = |extra: i16| {
            let measure = (self.right - READER_WRAP_SAFETY - line.x).max(1) as u32;
            let extra = extra.max(0) as u32;
            let short = if extra * 3 > measure {
                SHORT_BADNESS
            } else {
                0
            };
            u64::from(RAGGED_BADNESS + RAGGED_BADNESS * extra / measure + short)
        };
        if line.gaps == 0 {
            return match extra {
                0 => Some(0),
                extra if extra > 0 => Some(ragged(extra)),
                _ => None,
            };
        }
        if extra < 0 {
            return None;
        }
        let drawn = if line.height > 0 {
            justify_inline_extra(line.x, line.width, self.right, line.gaps)
        } else {
            justify_extra(line.x, line.width, self.right, line.gaps, space)
        };
        Some(match drawn {
            None if extra == 0 => 0,
            None => ragged(extra),
            Some(extra) => stretched(extra, line.gaps),
        })
    }

    /// The line the best path takes from node `node`, and the node it ends
    /// at.
    pub(crate) fn line_from(&self, node: usize) -> (Span, usize) {
        let to = self.node(node).1;
        let (first, from) = self.position(node);
        let span = if to < BREAK_NODES {
            Span {
                first,
                from,
                last: to - 1,
                head: None,
            }
        } else {
            let (word, _, head_end, drawn) = self.break_at(to - BREAK_NODES);
            Span {
                first,
                from,
                last: word,
                head: Some((head_end, drawn)),
            }
        };
        (span, to)
    }
}

fn style_bits(style: FontStyle) -> u16 {
    u16::from(style_marker_code(style) as u8 - b'0') << STYLE_SHIFT
}

fn style_from_bits(packed: u16) -> FontStyle {
    style_from_marker_code(char::from(b'0' + ((packed >> STYLE_SHIFT) & 3) as u8))
        .unwrap_or(FontStyle::Regular)
}

fn ends_with_dash(text: &str) -> bool {
    text.ends_with(['\u{2013}', '\u{2014}'])
}

/// FNV-1a, 64-bit: the cache keys a word by it (and its length).
fn word_hash(word: &str) -> u64 {
    word.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}
