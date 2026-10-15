use app_core::{ReaderSource, MAX_SD_CHAPTERS};
use display::font::{FontStyle, TypeSettings};
use heapless::String;
use proto::cache::{
    BlockRecord, BookV2SectionRecord, PageRecord, TocRecord, CACHE_KEY_BYTES, COVER_BYTES,
    COVER_HEIGHT, COVER_STRIDE, COVER_WIDTH, TOC_CHAPTER_RECORD_BYTES, TOC_CHAPTER_TITLE_BYTES,
};
use proto::text::{TextAlign, TextRole};

/// Resident slice of the on-disk catalog kept for the Library list. The full
/// catalog lives in CATALOG.BIN and is streamed a window at a time around the
/// selection, so the book count is bounded by the card, not by RAM. Sized a
/// little above `ui::render::library_visible_rows(true)` so ordinary scrolling stays
/// inside one loaded window and only crossings re-read the card.
pub(crate) const LIBRARY_WINDOW: usize = 16;
pub(crate) const MAX_FIRMWARE_FILES: usize = 16;
pub(crate) const MAX_SD_TOC_ITEMS: usize = 128;
/// Chapter-page map covering the whole on-disk TOC (not the 128-capped
/// resident/event arrays), so the current chapter tracks reading position
/// through a long book. One u16 page per chapter; 512 bytes resident.
pub(crate) const MAX_OVERVIEW_CHAPTERS: usize = 256;
/// Longest current-chapter title kept resident for the Home/sleep colophon;
/// read on demand from TOC.BIN as the chapter changes.
const MAX_CURRENT_CHAPTER_TITLE: usize = 60;
pub(crate) const MAX_CUSTOM_FONT_NAME: usize = proto::font_pack::FONT_PACK_MAX_NAME_BYTES;
pub(crate) const MAX_CUSTOM_FONT_FACES: usize = 12;
// ~14 pages per 16 KB section at the default size, so 320 covers ~4,500
// pages -- enough for very long books (e.g. HPMOR) to cache whole rather
// than tripping book_partial partway. Every spine item takes at least one
// section, so a book of many short files (essay collections, manuals split
// per section) needs as many sections as it has items. The table is
// resident at 4 bytes a section (`SectionTable`); the one stack copy is the
// spacing reflow's, on the shallow book-index load path.
pub(crate) const MAX_BOOK_SECTIONS: usize = 1024;
/// Sections a seed run (see `SeedRun`) can hold: a long chapter or two.
pub(crate) const MAX_SEED_SECTIONS: usize = 128;
/// A seed run's pages are numbered from here until the prefix reaches them.
/// Real pages stay far below (u16 page requests cap a book at 65,535 pages
/// anyway), so any page from `PROVISIONAL_PAGE_FLOOR` up is provisional
/// while a seed exists; the stretch below the base stands for the
/// unprepared pages just before the seed.
pub(crate) const PROVISIONAL_PAGE_FLOOR: u32 = 0x8000;
pub(crate) const PROVISIONAL_PAGE_BASE: u32 = 0xC000;
/// Seed section ids, clear of both section banks: a run of
/// `MAX_SEED_SECTIONS` per bank, so a rebuild's seed never overwrites the
/// sections the previous generation's index still names.
pub(crate) const SEED_SECTION_BASE: u16 = (2 * MAX_BOOK_SECTIONS) as u16;

/// Seed section ids for a book whose prefix sections live in `bank` (the
/// id of a bank's first section).
pub(crate) fn seed_section_base(bank: u16) -> u16 {
    SEED_SECTION_BASE + (bank / MAX_BOOK_SECTIONS as u16) * MAX_SEED_SECTIONS as u16
}
const MAX_PUBLISHED_CHAPTER_EVENTS: usize = MAX_SD_CHAPTERS;
// Titles only (hrefs/anchors are no longer stored), so 4 KB covers the full
// 128-item TOC at ~32 bytes per title and the saved RAM widens the stack
// region, which the EPUB open path runs close to.
pub(crate) const MAX_SD_TOC_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_READER_BLOCKS: usize = 384;
pub(crate) const MAX_READER_PAGES: usize = 96;
pub(crate) const MAX_READER_TEXT_BYTES: usize = 16_384;
/// TOC records the `text` buffer can hold at once for the Chapters overview;
/// longer TOCs are windowed around the visible rows (HPMOR-sized lists fit
/// whole, but e.g. a 322-entry trade-book TOC does not).
pub(crate) const TOC_WINDOW_CAPACITY: usize = MAX_READER_TEXT_BYTES / TOC_CHAPTER_RECORD_BYTES;
pub(crate) const MAX_READER_BLOCK_TEXT: usize = 768;
pub(crate) const EMPTY_BLOCK_RECORD: BlockRecord = BlockRecord {
    text_offset: 0,
    text_len: 0,
    line_count: 0,
    role: TextRole::Body,
    style: proto::text::FontStyle::Regular,
    align: TextAlign::Justify,
};
pub(crate) const EMPTY_PAGE_RECORD: PageRecord = PageRecord {
    first_block: 0,
    block_count: 0,
};
pub(crate) const EMPTY_TOC_RECORD: TocRecord = TocRecord {
    title_offset: 0,
    title_len: 0,
    href_offset: 0,
    href_len: 0,
    anchor_offset: 0,
    anchor_len: 0,
    level: 0,
    spine_index: -1,
};

/// A reading position that survives repagination. A type-settings or
/// orientation change lays the book out again and moves every page number,
/// so before the old pagination is replaced, the requested page is pinned
/// to its spine item and how far through that spine's pages it sat. The
/// rebuild resolves the pin against the new pagination, and the reader
/// keeps their place in the text rather than their page number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageAnchor {
    /// `(source_hash, source_size)` of the book.
    pub(crate) source: (u32, u32),
    /// The page as requested, numbered in the old pagination.
    pub(crate) page: u32,
    /// `(spine, fraction)` for the page, or None when the old pagination
    /// was consulted and the page number needs no translation.
    pub(crate) place: Option<(u16, u16)>,
}

impl PageAnchor {
    /// Pins `page` within one book's `sections` (reading order). The place
    /// is None when no section holds the page.
    pub(crate) fn in_sections<const N: usize>(
        source: (u32, u32),
        page: u32,
        sections: &SectionTable<N>,
    ) -> Self {
        let place = sections
            .iter()
            .find(|section| {
                page >= section.start_page
                    && page - section.start_page < u32::from(section.page_count)
            })
            .and_then(|holder| {
                let (start, pages) = spine_extent(sections, holder.spine)?;
                Some((holder.spine, page_fraction(page - start, pages)))
            });
        Self {
            source,
            page,
            place,
        }
    }
}

/// How far through `pages` the middle of page `offset` sits, in 1/65536ths.
/// Taking the middle makes an unchanged pagination map back to the exact
/// page (`page_at_fraction`) for any spine under 32,768 pages.
pub(crate) fn page_fraction(offset: u32, pages: u32) -> u16 {
    let pages = u64::from(pages.max(1));
    let offset = u64::from(offset).min(pages - 1);
    ((offset * 2 + 1) * 32_768 / pages) as u16
}

/// The page, of `pages`, that `fraction` (from `page_fraction`) falls on.
pub(crate) fn page_at_fraction(fraction: u16, pages: u32) -> u32 {
    ((u64::from(fraction) * u64::from(pages)) >> 16) as u32
}

/// First page and page count of `spine` within one book's `sections`.
fn spine_extent<const N: usize>(sections: &SectionTable<N>, spine: u16) -> Option<(u32, u32)> {
    let mut extent: Option<(u32, u32)> = None;
    for section in sections.iter().filter(|section| section.spine == spine) {
        let (_, pages) = extent.get_or_insert((section.start_page, 0));
        *pages += u32::from(section.page_count);
    }
    extent
}

/// One book's section index as held in RAM: 4 bytes a section instead of
/// the card record's 12. Sections are numbered contiguously from `base` and
/// each starts where the one before it ends (a book index is only accepted
/// that way), so ids and start pages are derived rather than stored. Walking
/// it in order yields full records; there are at most `MAX_BOOK_SECTIONS`.
#[derive(Clone, Copy)]
pub(crate) struct SectionTable<const N: usize = MAX_BOOK_SECTIONS> {
    base: u16,
    start_page: u32,
    end_page: u32,
    len: usize,
    entries: [SectionEntry; N],
}

/// A section's spine and page count; the top bit of `pages` marks a
/// partial section.
#[derive(Clone, Copy)]
struct SectionEntry {
    spine: u16,
    pages: u16,
}

const SECTION_PARTIAL: u16 = 0x8000;

impl<const N: usize> SectionTable<N> {
    pub(crate) const fn new() -> Self {
        Self {
            base: 0,
            start_page: 0,
            end_page: 0,
            len: 0,
            entries: [SectionEntry { spine: 0, pages: 0 }; N],
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len == N
    }

    pub(crate) fn clear(&mut self) {
        self.base = 0;
        self.start_page = 0;
        self.end_page = 0;
        self.len = 0;
    }

    /// Id of the first section; later ones follow contiguously.
    pub(crate) fn base(&self) -> u16 {
        self.base
    }

    /// The page after the last section's.
    pub(crate) fn end_page(&self) -> u32 {
        self.end_page
    }

    /// The id and first page the next section takes, starting a table at
    /// `base` and `start_page`.
    pub(crate) fn next_slot(&self, base: u16, start_page: u32) -> (u16, u32) {
        if self.len == 0 {
            (base, start_page)
        } else {
            (self.base.wrapping_add(self.len as u16), self.end_page)
        }
    }

    /// Drops every section from `len` on.
    pub(crate) fn truncate(&mut self, len: usize) {
        while self.len > len {
            self.len -= 1;
            self.end_page -= u32::from(self.entries[self.len].pages & !SECTION_PARTIAL);
        }
    }

    /// The section holding `page`.
    pub(crate) fn find_page(&self, page: u32) -> Option<BookV2SectionRecord> {
        if page < self.start_page || page >= self.end_page {
            return None;
        }
        self.iter().find(|section| {
            page >= section.start_page && page - section.start_page < u32::from(section.page_count)
        })
    }

    /// Appends the next section. False when the table is full or the record
    /// does not continue it: the next id, starting on the next page.
    pub(crate) fn push(&mut self, record: BookV2SectionRecord) -> bool {
        if self.is_full() || record.page_count >= SECTION_PARTIAL {
            return false;
        }
        if self.len == 0 {
            self.base = record.section;
            self.start_page = record.start_page;
            self.end_page = record.start_page;
        } else if usize::from(record.section) != usize::from(self.base) + self.len
            || record.start_page != self.end_page
        {
            return false;
        }
        let Some(end) = self.end_page.checked_add(u32::from(record.page_count)) else {
            return false;
        };
        self.entries[self.len] = SectionEntry {
            spine: record.spine,
            pages: record.page_count | if record.partial { SECTION_PARTIAL } else { 0 },
        };
        self.len += 1;
        self.end_page = end;
        true
    }

    /// Replaces the table with `records`, stopping at the first that does
    /// not continue it. False if any was left out.
    #[cfg(test)]
    pub(crate) fn set_records(&mut self, records: &[BookV2SectionRecord]) -> bool {
        self.clear();
        records.iter().all(|record| self.push(*record))
    }

    /// The sections in reading order, as full records.
    pub(crate) fn iter(&self) -> impl Iterator<Item = BookV2SectionRecord> + Clone + '_ {
        let base = self.base;
        self.entries[..self.len].iter().enumerate().scan(
            self.start_page,
            move |start, (index, entry)| {
                let page_count = entry.pages & !SECTION_PARTIAL;
                let record = BookV2SectionRecord {
                    section: base.wrapping_add(index as u16),
                    spine: entry.spine,
                    start_page: *start,
                    page_count,
                    partial: entry.pages & SECTION_PARTIAL != 0,
                };
                *start += u32::from(page_count);
                Some(record)
            },
        )
    }

    pub(crate) fn last(&self) -> Option<BookV2SectionRecord> {
        let index = self.len.checked_sub(1)?;
        let entry = self.entries[index];
        let page_count = entry.pages & !SECTION_PARTIAL;
        Some(BookV2SectionRecord {
            section: self.base.wrapping_add(index as u16),
            spine: entry.spine,
            start_page: self.end_page - u32::from(page_count),
            page_count,
            partial: entry.pages & SECTION_PARTIAL != 0,
        })
    }
}

/// Spines laid out ahead of the prefix, so a request deep into a book (a
/// relaid book's saved place, a chapter jump) need not wait for every spine
/// before it. Page numbers depend on every earlier spine, so the run's pages
/// are numbered from `PROVISIONAL_PAGE_BASE` until the prefix lays the same
/// spines out (layout is per spine, so they come out the same) and they take
/// their real numbers. The run is kept with the index after that, so a
/// provisional page saved or requested meanwhile still finds its place.
/// One per generation of the book's cache: a new run would renumber the
/// pages of the old.
#[derive(Clone, Copy)]
pub(crate) struct SeedRun {
    /// The run's first spine; None when this generation has none.
    pub(crate) first_spine: Option<u16>,
    /// The next spine the run would lay out; None once it reaches the end of
    /// the book.
    pub(crate) next_spine: Option<u16>,
    /// The run can take no more spines (its table filled).
    pub(crate) capped: bool,
    pub(crate) sections: SectionTable<MAX_SEED_SECTIONS>,
}

impl SeedRun {
    pub(crate) const fn new() -> Self {
        Self {
            first_spine: None,
            next_spine: None,
            capped: false,
            sections: SectionTable::new(),
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    /// Whether the run has laid out `spine` (it may have come out empty).
    pub(crate) fn laid_out(&self, spine: u16) -> bool {
        self.first_spine
            .is_some_and(|first| spine >= first && self.next_spine.is_none_or(|next| spine < next))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LibraryScanStatus {
    NotScanned,
    Scanning,
    Ready,
    Empty,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BookLoadStatus {
    Empty,
    Loading,
    Ready,
    Error,
}

pub(crate) struct LibraryBookEntry {
    pub(crate) display_name: String<64>,
    pub(crate) display_label: String<64>,
    pub(crate) open_name: String<16>,
    pub(crate) in_books_dir: bool,
    pub(crate) byte_size: u32,
    pub(crate) source_hash: u32,
}

impl LibraryBookEntry {
    pub(crate) const fn new() -> Self {
        Self {
            display_name: String::new(),
            display_label: String::new(),
            open_name: String::new(),
            in_books_dir: false,
            byte_size: 0,
            source_hash: 0,
        }
    }
}

pub(crate) struct FirmwareFileEntry {
    pub(crate) display_name: String<64>,
    pub(crate) open_name: String<16>,
    pub(crate) byte_size: u32,
}

impl FirmwareFileEntry {
    pub(crate) const fn new() -> Self {
        Self {
            display_name: String::new(),
            open_name: String::new(),
            byte_size: 0,
        }
    }
}

pub(crate) struct ReaderCover<'a> {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) stride: u16,
    pub(crate) bits: &'a [u8; COVER_BYTES],
}

pub(crate) struct TocItem<'a> {
    pub(crate) title: &'a str,
    pub(crate) level: u8,
    /// 1-based book page the chapter starts on; 0 when unknown.
    pub(crate) page: u32,
}

pub(crate) struct ReaderStore {
    pub(crate) status: LibraryScanStatus,
    /// Full book count across CATALOG.BIN (the source of truth), independent of
    /// what is resident.
    total: u16,
    /// Resident list window: `window[i]` is the book at absolute index
    /// `window_start + i`, for `i < window_len`.
    window: [LibraryBookEntry; LIBRARY_WINDOW],
    window_start: usize,
    window_len: usize,
    /// The one book currently being opened/read. Held apart from the list
    /// window so the reading path never depends on where the Library list
    /// happens to be scrolled; `catalog_entry` returns it for `active_index`.
    active_entry: LibraryBookEntry,
    active_index: Option<usize>,
    firmware_files: [FirmwareFileEntry; MAX_FIRMWARE_FILES],
    firmware_count: usize,
    pub(crate) current_index: Option<usize>,
    pub(crate) loaded_index: Option<usize>,
    pub(crate) loaded_chapter: u16,
    pub(crate) reader_status: BookLoadStatus,
    pub(crate) title: String<64>,
    pub(crate) author: String<64>,
    pub(crate) error: String<32>,
    pub(crate) cache_key: String<CACHE_KEY_BYTES>,
    pub(crate) cover_ready: bool,
    pub(crate) cover_width: u16,
    pub(crate) cover_height: u16,
    pub(crate) cover_bits: [u8; COVER_BYTES],
    pub(crate) cached_spine: u16,
    pub(crate) section_partial: bool,
    pub(crate) book_total_pages: u32,
    pub(crate) current_section_start_page: u32,
    pub(crate) current_section_page_count: u16,
    pub(crate) book_cache_ready: bool,
    pub(crate) book_cache_partial: bool,
    book_next_spine: Option<u16>,
    book_generated_toc: bool,
    pub(crate) book_sections: SectionTable,
    pub(crate) seed: SeedRun,
    /// Pin for the page a layout-changing load asked for, held across the
    /// rebuild's steps until that load completes (see `PageAnchor`).
    page_anchor: Option<PageAnchor>,
    /// Raw spine ordinal where the loaded book's main text starts, from its
    /// index header or the build that writes it.
    text_start_spine: Option<u16>,
    /// `(source_hash, source_size)` of a book being opened for the first
    /// time: its page-0 request means the start of the text. Spent with the
    /// load, like the page anchor.
    seek_text_start: Option<(u32, u32)>,
    pub(crate) toc_text: [u8; MAX_SD_TOC_TEXT_BYTES],
    pub(crate) toc_text_len: usize,
    pub(crate) toc: [TocRecord; MAX_SD_TOC_ITEMS],
    pub(crate) toc_page: [u16; MAX_SD_TOC_ITEMS],
    pub(crate) toc_count: usize,
    /// Full chapter count from TOC.BIN (the on-disk list), which can exceed
    /// the resident `toc_count`. The Chapters overview reads the full list.
    pub(crate) toc_total: usize,
    /// While the Chapters view is open, `text` holds the on-disk TOC records
    /// instead of section content; the reading section is reloaded on exit.
    /// The buffer fits `TOC_WINDOW_CAPACITY` records, so for longer TOCs it
    /// holds a window: `text` record `i` is chapter `toc_window_start + i`,
    /// for `i < toc_window_len`. Slid around the visible rows before each
    /// Chapters render, like the Library's catalog window.
    pub(crate) text_holds_toc: bool,
    pub(crate) toc_window_start: usize,
    pub(crate) toc_window_len: usize,
    /// Start page of every chapter in the book (chapter -> page_for_spine),
    /// filled once at open from TOC.BIN. Covers the whole TOC, so the current
    /// chapter never gets stuck at the 128-entry cap.
    pub(crate) chapter_page: [u16; MAX_OVERVIEW_CHAPTERS],
    /// Number of valid entries in `chapter_page`; independent of the overview's
    /// `toc_total` so the current-chapter map survives a Chapters visit.
    pub(crate) chapter_page_count: usize,
    /// `(source_hash, source_size, font_config, custom_font_identity)` the
    /// `chapter_page` map was built for. The book index reloads every section
    /// crossing, so this token keeps the map from being re-read from disk
    /// except on a new book, a repaginating settings change, or a custom pack
    /// replacement.
    pub(crate) chapter_page_token: (u32, u32, u16, u64),
    /// Current chapter and its title, resolved by the firmware from
    /// `chapter_page` + the reading page on each section load, for the
    /// Home/sleep colophon and the overview's starting selection.
    pub(crate) current_chapter: u16,
    pub(crate) current_chapter_title: String<MAX_CURRENT_CHAPTER_TITLE>,
    /// Source identity (hash, size) of the book `current_chapter_title` belongs
    /// to, so a colophon shows it only for that book -- the resolved title
    /// outlives a single load (it is also set on boot restore, before the book
    /// is opened, so wake-to-Home names the chapter without a full open).
    pub(crate) current_chapter_source: (u32, u32),
    pub(crate) text: [u8; MAX_READER_TEXT_BYTES],
    pub(crate) text_len: usize,
    pub(crate) blocks: [BlockRecord; MAX_READER_BLOCKS],
    pub(crate) block_styles: [FontStyle; MAX_READER_BLOCKS],
    pub(crate) block_spine: [u16; MAX_READER_BLOCKS],
    pub(crate) block_page_break_before: [bool; MAX_READER_BLOCKS],
    pub(crate) block_paragraph_end: [bool; MAX_READER_BLOCKS],
    /// True for a block that opens a paragraph (its opening line takes the
    /// first-line indent). Persisted rather than derived so a section that
    /// carries a half-finished paragraph in at its front keeps that
    /// continuation line flush left.
    pub(crate) block_paragraph_start: [bool; MAX_READER_BLOCKS],
    pub(crate) block_count: usize,
    pub(crate) pages: [PageRecord; MAX_READER_PAGES],
    pub(crate) page_spine: [u16; MAX_READER_PAGES],
    pub(crate) page_count: usize,
    type_settings: TypeSettings,
    /// Whether the current layout paginates into the portrait page box.
    portrait: bool,
    custom_font_available: bool,
    custom_font_identity: u64,
    custom_font_name: String<MAX_CUSTOM_FONT_NAME>,
    custom_font_faces: [proto::font_pack::FontPackFaceRecord; MAX_CUSTOM_FONT_FACES],
    custom_font_face_count: usize,
}

impl ReaderStore {
    pub(crate) const fn new() -> Self {
        Self {
            status: LibraryScanStatus::NotScanned,
            total: 0,
            window: [const { LibraryBookEntry::new() }; LIBRARY_WINDOW],
            window_start: 0,
            window_len: 0,
            active_entry: LibraryBookEntry::new(),
            active_index: None,
            firmware_files: [const { FirmwareFileEntry::new() }; MAX_FIRMWARE_FILES],
            firmware_count: 0,
            current_index: None,
            loaded_index: None,
            loaded_chapter: 0,
            reader_status: BookLoadStatus::Empty,
            title: String::new(),
            author: String::new(),
            error: String::new(),
            cache_key: String::new(),
            cover_ready: false,
            cover_width: COVER_WIDTH as u16,
            cover_height: COVER_HEIGHT as u16,
            cover_bits: [0; COVER_BYTES],
            cached_spine: 0,
            section_partial: false,
            book_total_pages: 0,
            current_section_start_page: 0,
            current_section_page_count: 0,
            book_cache_ready: false,
            book_cache_partial: false,
            book_next_spine: None,
            book_generated_toc: false,
            book_sections: SectionTable::new(),
            seed: SeedRun::new(),
            page_anchor: None,
            text_start_spine: None,
            seek_text_start: None,
            toc_text: [0; MAX_SD_TOC_TEXT_BYTES],
            toc_text_len: 0,
            toc: [EMPTY_TOC_RECORD; MAX_SD_TOC_ITEMS],
            toc_page: [0; MAX_SD_TOC_ITEMS],
            toc_count: 0,
            toc_total: 0,
            text_holds_toc: false,
            toc_window_start: 0,
            toc_window_len: 0,
            chapter_page: [0; MAX_OVERVIEW_CHAPTERS],
            chapter_page_count: 0,
            chapter_page_token: (0, 0, 0, 0),
            current_chapter: 0,
            current_chapter_title: String::new(),
            current_chapter_source: (0, 0),
            text: [0; MAX_READER_TEXT_BYTES],
            text_len: 0,
            blocks: [EMPTY_BLOCK_RECORD; MAX_READER_BLOCKS],
            block_styles: [FontStyle::Regular; MAX_READER_BLOCKS],
            block_spine: [0; MAX_READER_BLOCKS],
            block_page_break_before: [false; MAX_READER_BLOCKS],
            block_paragraph_end: [true; MAX_READER_BLOCKS],
            block_paragraph_start: [false; MAX_READER_BLOCKS],
            block_count: 0,
            pages: [EMPTY_PAGE_RECORD; MAX_READER_PAGES],
            page_spine: [0; MAX_READER_PAGES],
            page_count: 0,
            type_settings: TypeSettings::DEFAULT,
            portrait: false,
            custom_font_available: false,
            custom_font_identity: 0,
            custom_font_name: String::new(),
            custom_font_faces: [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES],
            custom_font_face_count: 0,
        }
    }

    pub(crate) fn clear_firmware_files(&mut self) {
        self.firmware_count = 0;
        for entry in &mut self.firmware_files {
            *entry = FirmwareFileEntry::new();
        }
    }

    pub(crate) fn push_firmware_file(&mut self, display_name: &str, open_name: &str, size: u32) {
        let Some(entry) = self.firmware_files.get_mut(self.firmware_count) else {
            return;
        };
        entry.display_name.clear();
        let _ = entry.display_name.push_str(display_name);
        entry.open_name.clear();
        let _ = entry.open_name.push_str(open_name);
        entry.byte_size = size;
        self.firmware_count += 1;
    }

    pub(crate) fn firmware_files(&self) -> &[FirmwareFileEntry] {
        &self.firmware_files[..self.firmware_count]
    }

    pub(crate) fn firmware_file(&self, index: usize) -> Option<&FirmwareFileEntry> {
        self.firmware_files().get(index)
    }

    pub(crate) fn set_custom_font(
        &mut self,
        name: Option<&str>,
        identity: u64,
        faces: &[proto::font_pack::FontPackFaceRecord],
    ) {
        self.custom_font_name.clear();
        if let Some(name) = name {
            let _ = self.custom_font_name.push_str(name);
            self.custom_font_available = true;
            self.custom_font_identity = identity;
            self.custom_font_face_count = faces.len().min(MAX_CUSTOM_FONT_FACES);
            self.custom_font_faces =
                [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES];
            self.custom_font_faces[..self.custom_font_face_count]
                .copy_from_slice(&faces[..self.custom_font_face_count]);
        } else {
            self.custom_font_available = false;
            self.custom_font_identity = 0;
            self.custom_font_face_count = 0;
            self.custom_font_faces =
                [proto::font_pack::FontPackFaceRecord::EMPTY; MAX_CUSTOM_FONT_FACES];
        }
    }

    pub(crate) fn custom_font_available(&self) -> bool {
        self.custom_font_available
    }

    pub(crate) fn custom_font_name(&self) -> &str {
        self.custom_font_name.as_str()
    }

    pub(crate) fn custom_font_identity(&self) -> u64 {
        if self.custom_font_available {
            self.custom_font_identity
        } else {
            0
        }
    }

    pub(crate) fn custom_font_face(
        &self,
        size_px: u8,
        style: u8,
    ) -> Option<proto::font_pack::FontPackFaceRecord> {
        self.custom_font_faces[..self.custom_font_face_count]
            .iter()
            .copied()
            .find(|face| face.size_px == size_px && face.style == style)
    }

    pub(crate) fn type_settings(&self) -> TypeSettings {
        self.type_settings
    }

    pub(crate) fn portrait(&self) -> bool {
        self.portrait
    }

    pub(crate) fn page_box(&self) -> ui::reading::PageBox {
        ui::reading::PageBox::for_portrait(self.portrait)
    }

    /// Adopt a new reading layout — type settings plus the page box — and
    /// drop the in-RAM section window's page coverage, so the next
    /// open/extend reloads the section under the new layout (a size or
    /// orientation change rebuilds it from the EPUB) instead of serving
    /// pages paginated under the old one.
    pub(crate) fn set_layout(&mut self, settings: TypeSettings, portrait: bool) {
        if self.type_settings == settings && self.portrait == portrait {
            return;
        }
        self.type_settings = settings;
        self.portrait = portrait;
        self.page_count = 0;
        self.current_section_page_count = 0;
        self.book_next_spine = None;
        self.seed.clear();
    }

    pub(crate) fn clear_catalog(&mut self) {
        self.total = 0;
        self.window_start = 0;
        self.window_len = 0;
        self.active_index = None;
        self.current_index = None;
    }

    pub(crate) fn catalog_count(&self) -> usize {
        self.total as usize
    }

    pub(crate) fn catalog_count_u16(&self) -> u16 {
        self.total
    }

    pub(crate) fn set_catalog_total(&mut self, total: u16) {
        self.total = total;
    }

    pub(crate) fn catalog_is_empty(&self) -> bool {
        self.total == 0
    }

    /// The resident list window and its absolute start, for the Library view.
    pub(crate) fn catalog_window(&self) -> &[LibraryBookEntry] {
        &self.window[..self.window_len]
    }

    pub(crate) fn catalog_window_start(&self) -> usize {
        self.window_start
    }

    /// True when the loaded window already covers `[start, start+len)`, so the
    /// firmware can skip a re-read while scrolling inside it.
    pub(crate) fn window_covers(&self, start: usize, len: usize) -> bool {
        self.window_len > 0
            && start >= self.window_start
            && start + len <= self.window_start + self.window_len
    }

    /// Begin filling a fresh window at `start`; `push_window_entry` appends.
    pub(crate) fn begin_window(&mut self, start: usize) {
        self.window_start = start;
        self.window_len = 0;
    }

    pub(crate) fn push_window_entry(
        &mut self,
        display_name: &str,
        open_name: &str,
        in_books_dir: bool,
        byte_size: u32,
        source_hash: u32,
        label_override: Option<&str>,
    ) {
        if self.window_len >= self.window.len() {
            return;
        }
        let slot = self.window_len;
        fill_entry(
            &mut self.window[slot],
            display_name,
            open_name,
            in_books_dir,
            byte_size,
            source_hash,
            label_override,
        );
        self.window_len += 1;
    }

    /// Adopt `index` as the active book whose entry the reading path reads
    /// through `catalog_entry`. Read once from CATALOG.BIN at open/restore.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_active_entry(
        &mut self,
        index: usize,
        display_name: &str,
        open_name: &str,
        in_books_dir: bool,
        byte_size: u32,
        source_hash: u32,
        label_override: Option<&str>,
    ) {
        fill_entry(
            &mut self.active_entry,
            display_name,
            open_name,
            in_books_dir,
            byte_size,
            source_hash,
            label_override,
        );
        self.active_index = Some(index);
    }

    /// Copy the loaded book's title into the resident catalog entries for
    /// `index` -- the list window entry when it is on screen, and the active
    /// entry when it is the active book -- so the list shows the real title
    /// right after an open and keeps showing it when the cursor moves on.
    fn note_loaded_title(&mut self, index: usize) {
        if self.title.is_empty() {
            return;
        }
        if self.active_index == Some(index) {
            copy_label(&mut self.active_entry.display_label, self.title.as_str());
        }
        if index >= self.window_start {
            let offset = index - self.window_start;
            if offset < self.window_len {
                copy_label(&mut self.window[offset].display_label, self.title.as_str());
            }
        }
    }

    pub(crate) fn catalog_entry(&self, index: usize) -> Option<&LibraryBookEntry> {
        if self.active_index == Some(index) {
            return Some(&self.active_entry);
        }
        if index >= self.window_start {
            if let Some(entry) = self.window.get(index - self.window_start) {
                if index - self.window_start < self.window_len {
                    return Some(entry);
                }
            }
        }
        None
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.active_index
    }

    pub(crate) fn selected_book_index(book_id: u32) -> Option<usize> {
        ReaderSource::from_book_id(book_id)
            .sd_index()
            .map(|index| index as usize)
    }

    pub(crate) fn source_identity(&self, book_id: u32) -> (u32, u32) {
        let Some(entry) =
            Self::selected_book_index(book_id).and_then(|index| self.catalog_entry(index))
        else {
            return (0, 0);
        };
        (entry.source_hash, entry.byte_size)
    }

    pub(crate) fn clear_toc(&mut self) {
        self.toc_text_len = 0;
        self.toc_count = 0;
        for (index, record) in self.toc.iter_mut().enumerate() {
            *record = EMPTY_TOC_RECORD;
            self.toc_page[index] = 0;
        }
    }

    pub(crate) fn clear_cover(&mut self) {
        self.cover_ready = false;
        self.cover_width = COVER_WIDTH as u16;
        self.cover_height = COVER_HEIGHT as u16;
        self.cover_bits.fill(0);
    }

    /// Expose the cover buffer for direct file reads. The cover is marked
    /// not-ready until [`Self::finish_cover_load`] validates it, so a failed
    /// read can never leave a half-written cover visible.
    pub(crate) fn cover_bits_mut(&mut self) -> &mut [u8; COVER_BYTES] {
        self.cover_ready = false;
        &mut self.cover_bits
    }

    pub(crate) fn finish_cover_load(&mut self, width: u16, height: u16) {
        self.cover_width = width;
        self.cover_height = height;
        self.cover_ready = true;
    }

    pub(crate) fn clear_lines(&mut self) {
        self.text_len = 0;
        self.block_count = 0;
        self.page_count = 0;
        self.section_partial = false;
        for (index, block) in self.blocks.iter_mut().enumerate() {
            *block = EMPTY_BLOCK_RECORD;
            self.block_styles[index] = FontStyle::Regular;
            self.block_spine[index] = 0;
            self.block_page_break_before[index] = false;
            self.block_paragraph_end[index] = true;
            self.block_paragraph_start[index] = false;
        }
        for (index, page) in self.pages.iter_mut().enumerate() {
            *page = EMPTY_PAGE_RECORD;
            self.page_spine[index] = 0;
        }
    }

    /// Keep only the blocks of the in-progress final page at the front of
    /// the store, rebasing their text. Lets an intermediate section flush on
    /// a whole-page boundary and carry the half-finished page into the next
    /// section, instead of writing it as a short, half-empty page the reader
    /// stops on. `first_block` is that page's first block; callers guarantee
    /// `0 < first_block < block_count`.
    pub(crate) fn carry_last_page(&mut self, first_block: usize) {
        if first_block == 0 || first_block >= self.block_count {
            return;
        }
        let text_start = self.blocks[first_block].text_offset as usize;
        let carried_blocks = self.block_count - first_block;
        let carried_text = self.text_len.saturating_sub(text_start);
        self.text.copy_within(text_start..self.text_len, 0);
        for offset in 0..carried_blocks {
            let src = first_block + offset;
            let mut record = self.blocks[src];
            record.text_offset = record.text_offset.saturating_sub(text_start as u32);
            self.blocks[offset] = record;
            self.block_styles[offset] = self.block_styles[src];
            self.block_spine[offset] = self.block_spine[src];
            self.block_page_break_before[offset] = self.block_page_break_before[src];
            self.block_paragraph_end[offset] = self.block_paragraph_end[src];
            self.block_paragraph_start[offset] = self.block_paragraph_start[src];
        }
        for index in carried_blocks..self.block_count {
            self.blocks[index] = EMPTY_BLOCK_RECORD;
            self.block_styles[index] = FontStyle::Regular;
            self.block_spine[index] = 0;
            self.block_page_break_before[index] = false;
            self.block_paragraph_end[index] = true;
            self.block_paragraph_start[index] = false;
        }
        self.block_count = carried_blocks;
        self.text_len = carried_text;
        self.page_count = 0;
        for (index, page) in self.pages.iter_mut().enumerate() {
            *page = EMPTY_PAGE_RECORD;
            self.page_spine[index] = 0;
        }
    }

    pub(crate) fn clear_book_index(&mut self) {
        self.book_next_spine = None;
        self.seed.clear();
        self.book_generated_toc = false;
        self.book_total_pages = 0;
        self.current_section_start_page = 0;
        self.current_section_page_count = 0;
        self.book_cache_ready = false;
        self.book_cache_partial = false;
        self.book_sections.clear();
        self.text_start_spine = None;
    }

    pub(crate) fn begin_book_load(&mut self) {
        self.loaded_index = None;
        self.reader_status = BookLoadStatus::Loading;
        self.title.clear();
        self.author.clear();
        self.error.clear();
        self.clear_toc();
        self.clear_lines();
        self.clear_book_index();
    }

    /// `Ready` means the requested page is readable, which a prepared
    /// prefix can already be: the rest of the book is prepared while the
    /// reader is idle (see the display task), and `book_next_spine` says
    /// where that resumes.
    pub(crate) fn finish_book_load(&mut self, index: usize, chapter: u16, status: BookLoadStatus) {
        if matches!(status, BookLoadStatus::Ready) {
            self.set_current_index(index);
        }
        if matches!(status, BookLoadStatus::Ready | BookLoadStatus::Error) {
            self.loaded_index = Some(index);
            self.loaded_chapter = chapter;
            // Bake the just-learned title into the resident list/active entries
            // so the Library keeps showing it once the cursor moves to another
            // book -- without waiting for the next window refill from the card.
            // A later refill re-reads the same title from the book's cache, so
            // the label also survives scrolling away and reboots.
            self.note_loaded_title(index);
        }
        self.reader_status = status;
    }

    pub(crate) fn set_reader_status(&mut self, status: BookLoadStatus) {
        self.reader_status = status;
    }

    pub(crate) fn set_reader_error(&mut self, message: &str) {
        self.error.clear();
        let _ = self.error.push_str(message);
    }

    pub(crate) fn set_cache_key(&mut self, key: &str) {
        self.cache_key.clear();
        let _ = self.cache_key.push_str(key);
    }

    pub(crate) fn set_book_labels(&mut self, title: &str, author: &str) {
        copy_string(&mut self.title, title);
        copy_string(&mut self.author, author);
    }

    pub(crate) fn page_capacity(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn block_capacity(&self) -> usize {
        self.blocks.len()
    }

    /// True once the section text arena has less than one max-length line of
    /// headroom left. The streaming builder flushes the section here so the
    /// next line lands in a fresh chunk; without this the arena overflows
    /// and `push_line_block` starts silently dropping the rest of the
    /// chapter (text is the tightest of the three section budgets, hit long
    /// before the page or block caps).
    pub(crate) fn text_capacity_reached(&self) -> bool {
        self.text_len + MAX_READER_BLOCK_TEXT > self.text.len()
    }

    pub(crate) fn block_count(&self) -> usize {
        self.block_count
    }

    pub(crate) fn can_hold_section(
        &self,
        page_count: usize,
        block_count: usize,
        text_bytes: usize,
    ) -> bool {
        page_count <= self.pages.len()
            && block_count <= self.blocks.len()
            && text_bytes <= self.text.len()
    }

    pub(crate) fn set_cached_page(&mut self, index: usize, page: PageRecord, spine: u16) -> bool {
        if index >= self.pages.len() {
            return false;
        }
        self.pages[index] = page;
        self.page_spine[index] = spine;
        true
    }

    pub(crate) fn set_cached_block(
        &mut self,
        index: usize,
        block: BlockRecord,
        style: FontStyle,
        spine: u16,
    ) -> bool {
        if index >= self.blocks.len() {
            return false;
        }
        self.blocks[index] = block;
        self.block_styles[index] = style;
        self.block_spine[index] = spine;
        self.block_page_break_before[index] =
            should_break_before_block(block.role, self.blocks.get(index.wrapping_sub(1)));
        true
    }

    pub(crate) fn set_cached_paragraph_end(&mut self, index: usize, paragraph_end: bool) -> bool {
        let Some(slot) = self.block_paragraph_end.get_mut(index) else {
            return false;
        };
        *slot = paragraph_end;
        true
    }

    pub(crate) fn set_cached_paragraph_start(
        &mut self,
        index: usize,
        paragraph_start: bool,
    ) -> bool {
        let Some(slot) = self.block_paragraph_start.get_mut(index) else {
            return false;
        };
        *slot = paragraph_start;
        true
    }

    pub(crate) fn mark_last_block_paragraph_end(&mut self) {
        if self.block_count > 0 {
            self.block_paragraph_end[self.block_count - 1] = true;
        }
    }

    pub(crate) fn cached_text_mut(&mut self, text_bytes: usize) -> Option<&mut [u8]> {
        self.text.get_mut(..text_bytes)
    }

    pub(crate) fn finish_cached_section(
        &mut self,
        spine: u16,
        page_count: usize,
        block_count: usize,
        text_len: usize,
        partial: bool,
    ) {
        self.page_count = page_count;
        self.block_count = block_count;
        self.text_len = text_len;
        self.cached_spine = spine;
        self.section_partial = partial;
        // A real section now occupies the text buffer, replacing any TOC the
        // overview had loaded there.
        self.text_holds_toc = false;
        self.current_section_page_count = page_count.min(u16::MAX as usize) as u16;
    }

    pub(crate) fn set_section_partial(&mut self, partial: bool) {
        self.section_partial = partial;
    }

    pub(crate) fn set_cached_spine(&mut self, spine: u16) {
        self.cached_spine = spine;
    }

    /// Test fixture: install `sections` as the book's index.
    #[cfg(test)]
    pub(crate) fn set_book_index(
        &mut self,
        total_pages: u32,
        partial: bool,
        sections: &[BookV2SectionRecord],
    ) {
        let _ = self.book_sections.set_records(sections);
        self.commit_book_index(total_pages, partial);
    }

    /// Adopt the resident section table (filled in place by an index load or
    /// a build) as the book's index.
    pub(crate) fn commit_book_index(&mut self, total_pages: u32, partial: bool) {
        self.book_next_spine = None;
        // A seed run can be all there is so far: no prefix pages yet.
        self.book_total_pages = if self.book_sections.is_empty() {
            0
        } else {
            total_pages.max(1)
        };
        self.book_cache_ready = true;
        self.book_cache_partial = partial;
    }

    pub(crate) fn book_generated_toc(&self) -> bool {
        self.book_generated_toc
    }

    pub(crate) fn set_book_generated_toc(&mut self, generated: bool) {
        self.book_generated_toc = generated;
    }

    pub(crate) fn book_next_spine(&self) -> Option<u16> {
        self.book_next_spine
    }

    pub(crate) fn set_book_next_spine(&mut self, next: Option<u16>) {
        if self.book_next_spine != next {
            self.chapter_page_count = 0;
        }
        self.book_next_spine = next;
    }

    /// Where `spine` starts, in the prefix or else (provisionally) in the
    /// seed run.
    pub(crate) fn known_page_for_spine(&self, spine: u16) -> Option<u32> {
        self.book_sections
            .iter()
            .find(|section| section.spine == spine)
            .or_else(|| {
                self.seed
                    .sections
                    .iter()
                    .find(|section| section.spine == spine)
            })
            .map(|section| section.start_page)
    }

    /// Whether `spine` is still to be laid out: past the prefix and not in
    /// the seed run.
    fn spine_pending(&self, spine: u16) -> bool {
        self.book_next_spine.is_some_and(|next| spine >= next) && !self.seed.laid_out(spine)
    }

    /// Whether `page` is numbered provisionally, in the seed run's range.
    pub(crate) fn is_provisional(&self, page: u32) -> bool {
        self.seed.first_spine.is_some() && page >= PROVISIONAL_PAGE_FLOOR
    }

    /// The number `page` goes by now: a provisional page whose spine the
    /// prefix has since laid out takes its real number; any other page
    /// keeps its own.
    pub(crate) fn settled_page(&self, page: u32) -> u32 {
        self.real_page_for_provisional(page).unwrap_or(page)
    }

    fn real_page_for_provisional(&self, page: u32) -> Option<u32> {
        if !self.is_provisional(page) {
            return None;
        }
        if page < PROVISIONAL_PAGE_BASE {
            // Counted back from where the run starts: its first spine's
            // start in the prefix, or the prefix's end once everything
            // before that spine is laid out.
            let first = self.seed.sections.iter().next()?.spine;
            let start = spine_extent(&self.book_sections, first)
                .map(|(start, _)| start)
                .or_else(|| {
                    self.book_next_spine
                        .is_some_and(|next| next >= first)
                        .then_some(self.book_total_pages)
                })?;
            return Some(start.saturating_sub(PROVISIONAL_PAGE_BASE - page));
        }
        let page = if self.seed.next_spine.is_none() {
            // A run that reached the end of the book clamps like one.
            page.min(self.seed.sections.end_page().checked_sub(1)?)
        } else {
            page
        };
        let holder = self.seed.sections.find_page(page)?;
        let (seed_start, _) = spine_extent(&self.seed.sections, holder.spine)?;
        let (start, pages) = spine_extent(&self.book_sections, holder.spine)?;
        Some(start + (page - seed_start).min(pages.saturating_sub(1)))
    }

    /// `(spine, fraction)` for `page` in the prefix or the seed run.
    fn place_of_page(&self, page: u32) -> Option<(u16, u16)> {
        let page = self.settled_page(page);
        let (sections_place, seed_place) = (
            PageAnchor::in_sections((0, 0), page, &self.book_sections).place,
            PageAnchor::in_sections((0, 0), page, &self.seed.sections).place,
        );
        sections_place.or(seed_place)
    }

    /// The spine a request needs laid out next, when it names one beyond
    /// what is prepared: a chapter, the text start of a first open, the
    /// pinned place of a relaid page, or a provisional page past the seed
    /// run. None for a plain page number, which only the prefix can reach.
    pub(crate) fn request_spine(&self, page: u32, spine: Option<u16>) -> Option<u16> {
        if spine.is_some() {
            return spine;
        }
        if let Some(start) = self.sought_text_start(page) {
            return Some(start);
        }
        if let Some((spine, _)) = self.anchored_place(page) {
            return Some(spine);
        }
        if self.is_provisional(page)
            && page >= self.seed.sections.end_page().max(PROVISIONAL_PAGE_BASE)
        {
            return self.seed.next_spine;
        }
        None
    }

    pub(crate) fn requested_page(&self, page: u32, spine: Option<u16>) -> Option<u32> {
        if let Some(spine) = spine {
            if let Some(page) = self.known_page_for_spine(spine) {
                return Some(page);
            }
            if self.spine_pending(spine) {
                return None;
            }
        } else {
            if let Some(start) = self.sought_text_start(page) {
                match self.known_page_for_spine(start) {
                    // Only a start in the book's first quarter: a marker that
                    // deep is likelier wrong than that much front matter.
                    Some(first)
                        if self.book_next_spine.is_some()
                            || first.saturating_mul(4) <= self.book_total_pages =>
                    {
                        return Some(first);
                    }
                    None if self.spine_pending(start) => {
                        return None;
                    }
                    _ => {}
                }
            }
            if let Some(place) = self.anchored_place(page) {
                return self.page_for_place(place);
            }
        }
        if self.is_provisional(page) {
            if let Some(real) = self.real_page_for_provisional(page) {
                return Some(real);
            }
            // Still only in the seed run, or not laid out yet.
            let held = if self.seed.next_spine.is_none() {
                page.min(self.seed.sections.end_page().saturating_sub(1))
            } else {
                page
            };
            return self.seed.sections.find_page(held).map(|_| held);
        }
        if page < self.book_total_pages {
            Some(page)
        } else if self.book_next_spine.is_none() && self.book_total_pages > 0 {
            Some(self.book_total_pages - 1)
        } else {
            None
        }
    }

    /// Whether the old pagination was already consulted for this request.
    pub(crate) fn anchors(&self, source: (u32, u32), page: u32) -> bool {
        self.page_anchor
            .is_some_and(|anchor| anchor.source == source && anchor.page == page)
    }

    pub(crate) fn set_page_anchor(&mut self, anchor: Option<PageAnchor>) {
        self.page_anchor = anchor;
    }

    /// Pin `page` in the resident pagination before it is laid out again,
    /// unless this request is already pinned.
    pub(crate) fn anchor_resident_page(&mut self, source: (u32, u32), page: u32) {
        if !self.anchors(source, page) {
            self.page_anchor = Some(PageAnchor {
                source,
                page,
                place: self.place_of_page(page),
            });
        }
    }

    /// The pinned place for a request of `page` on the active book.
    fn anchored_place(&self, page: u32) -> Option<(u16, u16)> {
        let anchor = self.page_anchor?;
        (anchor.page == page && self.active_source() == Some(anchor.source))
            .then_some(anchor.place)
            .flatten()
    }

    fn active_source(&self) -> Option<(u32, u32)> {
        self.active_index
            .map(|_| (self.active_entry.source_hash, self.active_entry.byte_size))
    }

    pub(crate) fn text_start_spine(&self) -> Option<u16> {
        self.text_start_spine
    }

    pub(crate) fn set_text_start_spine(&mut self, spine: Option<u16>) {
        self.text_start_spine = spine;
    }

    /// The book `source` is opening for the first time; its page-0 request
    /// resolves to the start of the text once that is known.
    pub(crate) fn seek_text_start(&mut self, source: (u32, u32)) {
        self.seek_text_start = Some(source);
    }

    /// The text-start spine a request of `page` on the active book seeks.
    fn sought_text_start(&self, page: u32) -> Option<u16> {
        (page == 0
            && self.seek_text_start.is_some()
            && self.seek_text_start == self.active_source())
        .then_some(self.text_start_spine)
        .flatten()
    }

    /// A reader load finished: its page pin and first-open seek are spent,
    /// so a later request for the same page number means that page.
    pub(crate) fn end_reader_request(&mut self) {
        self.page_anchor = None;
        self.seek_text_start = None;
    }

    /// Where a pinned place lands in the resident pagination. None until
    /// its spine is laid out; a spine that laid out to no pages lands where
    /// the following text starts.
    fn page_for_place(&self, (spine, fraction): (u16, u16)) -> Option<u32> {
        let sections = &self.book_sections;
        if let Some((start, pages)) =
            spine_extent(sections, spine).or_else(|| spine_extent(&self.seed.sections, spine))
        {
            return Some(start + page_at_fraction(fraction, pages));
        }
        if self.seed.laid_out(spine) && self.book_next_spine.is_some_and(|next| spine >= next) {
            // Laid out empty in the seed run: the following text there.
            return self
                .seed
                .sections
                .iter()
                .find(|section| section.spine > spine)
                .map(|section| section.start_page);
        }
        if self.spine_pending(spine) {
            return None;
        }
        sections
            .iter()
            .find(|section| section.spine > spine)
            .map(|section| section.start_page)
            .or(self.book_total_pages.checked_sub(1))
    }

    pub(crate) fn set_current_section_range(&mut self, start_page: u32, page_count: usize) {
        self.current_section_start_page = start_page;
        self.current_section_page_count = page_count.min(u16::MAX as usize) as u16;
    }

    /// True when `global_page` of catalog entry `index` is already
    /// renderable from the loaded in-RAM section window, so an open or
    /// extend request needs no SD session at all. Partial sections keep
    /// going to SD so the bounded prefix can be regrown.
    pub(crate) fn covers_global_page(&self, index: usize, global_page: u32) -> bool {
        let global_page = self.settled_page(global_page);
        self.loaded_index == Some(index)
            && matches!(self.reader_status, BookLoadStatus::Ready)
            && !self.text_holds_toc
            && !self.section_partial
            && self.page_count > 0
            && global_page >= self.current_section_start_page
            && global_page
                < self
                    .current_section_start_page
                    .saturating_add(self.current_section_page_count as u32)
    }

    /// Whether a short section-cache read can satisfy the page, without EPUB
    /// preparation or its loading plate. The section need not be resident,
    /// and the book need only be prepared as far as the page.
    pub(crate) fn has_prepared_page(&self, index: usize, global_page: u32) -> bool {
        let global_page = self.settled_page(global_page);
        self.loaded_index == Some(index)
            && self.reader_status == BookLoadStatus::Ready
            && (global_page < self.book_total_pages || self.is_provisional(global_page))
            && self.section_for_global_page(global_page).is_some()
    }

    pub(crate) fn local_page_for_global(&self, global_page: u32) -> usize {
        self.settled_page(global_page)
            .saturating_sub(self.current_section_start_page)
            .min(self.current_section_page_count.saturating_sub(1) as u32) as usize
    }

    pub(crate) fn section_for_global_page(&self, global_page: u32) -> Option<BookV2SectionRecord> {
        let global_page = self.settled_page(global_page);
        if self.is_provisional(global_page) {
            return self.seed.sections.find_page(global_page);
        }
        self.book_sections
            .iter()
            .find(|section| {
                let start = section.start_page;
                let end = start.saturating_add(section.page_count.max(1) as u32);
                global_page >= start && global_page < end
            })
            .or_else(|| {
                if self.book_next_spine.is_some() {
                    return None;
                }
                self.book_sections
                    .last()
                    .filter(|section| global_page >= section.start_page)
            })
    }

    /// Page position within the chapter (spine item) containing
    /// `global_page`, as (one-based page in chapter, chapter page total).
    /// A long chapter now spans several cache sections, so the footer
    /// counter must aggregate every section sharing the page's spine rather
    /// than read a single section -- otherwise it resets mid-chapter at each
    /// chunk boundary. Returns None when there is no book index to aggregate
    /// (single in-RAM section), letting the caller fall back.
    pub(crate) fn chapter_page_position(&self, global_page: u32) -> Option<(u32, u32)> {
        let global_page = self.settled_page(global_page);
        let spine = self.section_for_global_page(global_page)?.spine;
        let (start, total) = if self.is_provisional(global_page) {
            spine_extent(&self.seed.sections, spine)?
        } else {
            spine_extent(&self.book_sections, spine)?
        };
        if total == 0 {
            return None;
        }
        let current = global_page
            .saturating_sub(start)
            .saturating_add(1)
            .min(total);
        Some((current, total))
    }

    pub(crate) fn block_text(&self, index: usize) -> &str {
        let Some(record) = self.blocks.get(index) else {
            return "";
        };
        let start = record.text_offset as usize;
        let end = start.saturating_add(record.text_len as usize);
        core::str::from_utf8(self.text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn block_record(&self, index: usize) -> Option<BlockRecord> {
        self.blocks
            .get(index)
            .copied()
            .filter(|_| index < self.block_count)
    }

    pub(crate) fn block_style(&self, index: usize) -> FontStyle {
        self.block_styles
            .get(index)
            .copied()
            .unwrap_or(FontStyle::Regular)
    }

    pub(crate) fn advertised_page_count(&self) -> u32 {
        self.book_total_pages.max(self.page_count.max(1) as u32)
    }

    pub(crate) fn toc_title(&self, index: usize) -> &str {
        let Some(record) = self.toc.get(index) else {
            return "";
        };
        let start = record.title_offset as usize;
        let end = start.saturating_add(record.title_len as usize);
        core::str::from_utf8(self.toc_text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn toc_count(&self) -> usize {
        self.toc_count
    }

    pub(crate) fn toc_item(&self, index: usize) -> Option<TocItem<'_>> {
        if index >= self.toc_count {
            return None;
        }
        // Unknown or provisional pages go unnumbered (0).
        let page = u32::from(self.toc_page[index]);
        Some(TocItem {
            title: self.toc_title(index),
            level: self.toc[index].level.max(1),
            page: if page >= PROVISIONAL_PAGE_FLOOR {
                0
            } else {
                page + 1
            },
        })
    }

    pub(crate) fn push_toc_record(&mut self, title: &str, level: u8, spine_index: i16) -> bool {
        if self.toc_count >= self.toc.len() {
            return false;
        }
        let title_offset = self.toc_text_len;
        if !self.append_toc_text(title) {
            return false;
        }
        // The spine target is resolved before push and carried by the record,
        // so href/anchor text is never read back. The whole toc_text budget
        // goes to titles; records keep empty href/anchor ranges so the cache
        // format stays unchanged.
        let empty_offset = self.toc_text_len as u32;
        self.toc[self.toc_count] = TocRecord {
            title_offset: title_offset as u32,
            title_len: title.len().min(u16::MAX as usize) as u16,
            href_offset: empty_offset,
            href_len: 0,
            anchor_offset: empty_offset,
            anchor_len: 0,
            level,
            spine_index,
        };
        self.toc_page[self.toc_count] = 0;
        self.toc_count += 1;
        true
    }

    /// Mark `text` as holding a window of the on-disk TOC records, loaded for
    /// the Chapters overview: `len` records starting at absolute chapter
    /// `start`, out of `total` on disk. The reading section is reloaded on
    /// exit because its content was overwritten.
    pub(crate) fn set_toc_window(&mut self, start: usize, len: usize, total: usize) {
        self.toc_window_start = start;
        self.toc_window_len = len;
        self.toc_total = total;
        self.text_holds_toc = true;
    }

    pub(crate) fn text_holds_toc(&self) -> bool {
        self.text_holds_toc
    }

    pub(crate) fn toc_window_start(&self) -> usize {
        self.toc_window_start
    }

    /// Whether the resident TOC window covers `need` chapters from absolute
    /// index `start` (clamped to the on-disk total).
    pub(crate) fn toc_window_covers(&self, start: usize, need: usize) -> bool {
        let end = (start + need).min(self.toc_total);
        self.text_holds_toc
            && start >= self.toc_window_start
            && end <= self.toc_window_start + self.toc_window_len
    }

    /// TOC record byte range in `text` for absolute chapter `index`, when the
    /// resident window holds it.
    fn toc_record_base(&self, index: usize) -> Option<usize> {
        if !self.text_holds_toc {
            return None;
        }
        let rel = index.checked_sub(self.toc_window_start)?;
        if rel >= self.toc_window_len {
            return None;
        }
        let base = rel * TOC_CHAPTER_RECORD_BYTES;
        (base + TOC_CHAPTER_RECORD_BYTES <= self.text.len()).then_some(base)
    }

    /// Chapters to show in the overview: the full on-disk count when the TOC
    /// is loaded, else the resident count.
    pub(crate) fn overview_chapter_count(&self) -> usize {
        if self.text_holds_toc {
            self.toc_total
        } else {
            self.toc_count
        }
    }

    /// Title of overview chapter `index` (absolute), read straight from the
    /// TOC records in `text` (borrowed, no copy).
    pub(crate) fn overview_title_at(&self, index: usize) -> &str {
        let Some(base) = self.toc_record_base(index) else {
            return "";
        };
        let title_len = (self.text[base + 3] as usize).min(TOC_CHAPTER_TITLE_BYTES);
        core::str::from_utf8(&self.text[base + 4..base + 4 + title_len]).unwrap_or("")
    }

    pub(crate) fn overview_level_at(&self, index: usize) -> u8 {
        match self.toc_record_base(index) {
            Some(base) => self.text[base + 2],
            None => 1,
        }
    }

    pub(crate) fn overview_spine_at(&self, index: usize) -> i16 {
        match self.toc_record_base(index) {
            Some(base) => i16::from_le_bytes([self.text[base], self.text[base + 1]]),
            None => -1,
        }
    }

    /// Global page a chapter starts on, computed from the section index by
    /// its spine -- so no resident chapter-page array is needed.
    /// Where `spine` starts: in the prefix, else provisionally in the seed
    /// run, else `u32::MAX` while it is still to come (0 in a finished book).
    pub(crate) fn page_for_spine(&self, spine: u16) -> u32 {
        self.known_page_for_spine(spine).unwrap_or_else(|| {
            if self.book_next_spine.is_some() {
                u32::MAX
            } else {
                0
            }
        })
    }

    /// A chapter's page number for the overview, or 0 (shown without one)
    /// while its page is unknown or only provisional.
    pub(crate) fn overview_page_at(&self, index: usize) -> u16 {
        let spine = self.overview_spine_at(index);
        if spine < 0 {
            return 0;
        }
        let page = self.page_for_spine(spine as u16);
        if page >= PROVISIONAL_PAGE_FLOOR {
            0
        } else {
            page as u16
        }
    }

    /// The chapter a page falls in, over the full chapter-page map -- so it
    /// keeps advancing past the 128-entry resident/event caps.
    ///
    /// `chapter_page` is not strictly increasing: several TOC chapters can
    /// share one spine (so they share a start page), and an entry with no
    /// resolvable spine is 0. So we cannot stop at the first entry past the
    /// page. Instead take the greatest start page not beyond `page`, then name
    /// the FIRST chapter at that page -- the chapter that opens the spine the
    /// reader is in, rather than the last one swept up by a plateau of zeros.
    pub(crate) fn current_chapter_for_page(&self, page: u32) -> u16 {
        let count = self.chapter_page_count.min(MAX_OVERVIEW_CHAPTERS);
        let mut best_page = 0u32;
        for index in 0..count {
            let start = u32::from(self.chapter_page[index]);
            if start <= page && start >= best_page {
                best_page = start;
            }
        }
        for index in 0..count {
            if u32::from(self.chapter_page[index]) == best_page {
                return index as u16;
            }
        }
        0
    }

    /// The chapter `page` falls in for the book `source`, when the resident
    /// chapter-page map belongs to that book.
    pub(crate) fn chapter_for_page_of(&self, source: (u32, u32), page: u32) -> Option<u16> {
        (self.chapter_page_count > 0
            && (self.chapter_page_token.0, self.chapter_page_token.1) == source)
            .then(|| self.current_chapter_for_page(page))
    }

    pub(crate) fn set_current_chapter(&mut self, chapter: u16, title: &str, source: (u32, u32)) {
        self.current_chapter = chapter;
        self.current_chapter_source = source;
        self.current_chapter_title.clear();
        for ch in title.chars() {
            if self.current_chapter_title.push(ch).is_err() {
                break;
            }
        }
    }

    pub(crate) fn current_chapter(&self) -> u16 {
        self.current_chapter
    }

    pub(crate) fn current_chapter_title(&self) -> &str {
        self.current_chapter_title.as_str()
    }

    /// Source identity of the book `current_chapter_title` was resolved for.
    pub(crate) fn current_chapter_source(&self) -> (u32, u32) {
        self.current_chapter_source
    }

    fn append_toc_text(&mut self, value: &str) -> bool {
        let bytes = value.as_bytes();
        if self.toc_text_len + bytes.len() > self.toc_text.len()
            || self.toc_text_len > u16::MAX as usize
            || bytes.len() > u16::MAX as usize
        {
            return false;
        }
        self.toc_text[self.toc_text_len..self.toc_text_len + bytes.len()].copy_from_slice(bytes);
        self.toc_text_len += bytes.len();
        true
    }

    pub(crate) fn active_book_labels<'a>(
        &'a self,
        book_id: u32,
        fallback_title: &'a str,
        fallback_author: &'a str,
    ) -> (&'a str, &'a str) {
        if !ReaderSource::from_book_id(book_id).is_sd() {
            return (fallback_title, fallback_author);
        }
        if self.reader_status == BookLoadStatus::Ready
            && self.loaded_index == Self::selected_book_index(book_id)
        {
            let title = if self.title.is_empty() {
                fallback_title
            } else {
                self.title.as_str()
            };
            let author = if self.author.is_empty() {
                fallback_author
            } else {
                self.author.as_str()
            };
            return (title, author);
        }
        Self::selected_book_index(book_id)
            .and_then(|index| self.catalog_entry(index))
            .map(|entry| (entry.display_label.as_str(), ""))
            .unwrap_or((fallback_title, fallback_author))
    }

    pub(crate) fn selected_cover(&self, book_id: u32) -> Option<ReaderCover<'_>> {
        if !ReaderSource::from_book_id(book_id).is_sd()
            || self.current_index != Self::selected_book_index(book_id)
            || !self.cover_ready
        {
            return None;
        }
        Some(ReaderCover {
            width: self.cover_width,
            height: self.cover_height,
            stride: COVER_STRIDE as u16,
            bits: &self.cover_bits,
        })
    }

    pub(crate) fn reader_status(&self) -> BookLoadStatus {
        self.reader_status
    }

    pub(crate) fn reader_error(&self) -> &str {
        self.error.as_str()
    }

    pub(crate) fn chapter_count_for_ui(&self) -> u16 {
        // The full on-disk count (when known) drives the overview's
        // selection range; it can run past the 128-entry resident/event
        // caps, so only the u8 message width bounds it.
        let count = if self.toc_total > 0 {
            self.toc_total
        } else if self.toc_count > 0 {
            self.toc_count
        } else {
            self.book_sections.len()
        };
        count.min(u16::MAX as usize).max(1) as u16
    }

    pub(crate) fn set_current_index(&mut self, index: usize) {
        if index < self.total as usize {
            self.current_index = Some(index);
        }
    }

    pub(crate) fn push_line_block(
        &mut self,
        line: &str,
        style: FontStyle,
        role: TextRole,
        align: TextAlign,
        paragraph_end: bool,
        spine_index: u16,
    ) -> bool {
        // ASCII trim only: a line that is just a no-break space is a
        // deliberate blank line (`<p>&nbsp;</p>` spacers), not empty.
        let line = line.trim_matches(|ch: char| ch.is_ascii_whitespace());
        if line.is_empty() {
            return true;
        }
        if self.block_count >= self.blocks.len() {
            // Full: report it so the builder flushes the section and retries
            // the line in a fresh arena instead of losing it.
            return false;
        }
        let start = self.text_len;
        let bytes = line.as_bytes();
        if start + bytes.len() > self.text.len() || bytes.len() > u16::MAX as usize {
            return false;
        }
        self.text[start..start + bytes.len()].copy_from_slice(bytes);
        self.text_len += bytes.len();
        self.blocks[self.block_count] = BlockRecord {
            text_offset: start as u32,
            text_len: bytes.len() as u16,
            line_count: 1,
            role,
            style: proto_style_for_display_style(style),
            align,
        };
        self.block_styles[self.block_count] = style;
        self.block_spine[self.block_count] = spine_index;
        self.block_page_break_before[self.block_count] =
            should_break_before_block(role, self.blocks.get(self.block_count.wrapping_sub(1)));
        self.block_paragraph_end[self.block_count] = paragraph_end;
        // A line opens a paragraph when the previous block closed one (and the
        // first block always does). This mirrors the indent budget the build
        // wrap charged this line, so draw and pagination agree.
        self.block_paragraph_start[self.block_count] = self.block_count == 0
            || self
                .block_paragraph_end
                .get(self.block_count.wrapping_sub(1))
                .copied()
                .unwrap_or(true);
        self.block_count += 1;
        true
    }
}

/// Populate a catalog entry slot from a record's fields, deriving the display
/// label. Shared by the list window and the active-book entry; both are read
/// straight from CATALOG.BIN, which already carries the stored `source_hash`.
///
/// `label_override` is the EPUB title saved when the book was last opened: when
/// present it becomes the display label, so a book whose on-disk name can't
/// carry a real title (an 8.3 upload name) still reads as its title. With no
/// override the label falls back to the prettified file stem.
fn fill_entry(
    entry: &mut LibraryBookEntry,
    display_name: &str,
    open_name: &str,
    in_books_dir: bool,
    byte_size: u32,
    source_hash: u32,
    label_override: Option<&str>,
) {
    entry.display_name.clear();
    entry.display_label.clear();
    entry.open_name.clear();
    let _ = entry.display_name.push_str(display_name);
    match label_override {
        Some(label) if !label.is_empty() => {
            let _ = entry.display_label.push_str(label);
        }
        _ => push_catalog_label(display_name, open_name, &mut entry.display_label),
    }
    let _ = entry.open_name.push_str(open_name);
    entry.in_books_dir = in_books_dir;
    entry.byte_size = byte_size;
    entry.source_hash = source_hash;
}

/// Overwrite a resident entry's display label with `label`. Used to bake a
/// freshly-loaded book's title into the list without a card re-read.
fn copy_label(out: &mut String<64>, label: &str) {
    out.clear();
    let _ = out.push_str(label);
}

/// The list label a catalog record shows, derived from its file name. Exposed
/// for the streamed browser-shelf listing, which has only the on-disk record.
pub(crate) fn derive_catalog_label(display_name: &str, open_name: &str, out: &mut String<64>) {
    push_catalog_label(display_name, open_name, out);
}

fn push_catalog_label(display_name: &str, open_name: &str, out: &mut String<64>) {
    if open_name.eq_ignore_ascii_case("HPMOR.EPU") || open_name.eq_ignore_ascii_case("HPMOR.EPUB") {
        let _ = out.push_str("Harry Potter and the Methods of Rationality");
        return;
    }

    let file_name = display_name
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(display_name);
    let stem = strip_epub_suffix(file_name).unwrap_or(file_name);
    proto::storage::push_pretty_file_stem(stem, out);
    if out.is_empty() {
        let _ = out.push_str(display_name);
    }
}

fn strip_epub_suffix(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    if bytes.len() >= 5 && bytes[bytes.len() - 5..].eq_ignore_ascii_case(b".epub") {
        return Some(&name[..name.len() - 5]);
    }
    if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".epu") {
        return Some(&name[..name.len() - 4]);
    }
    None
}

fn should_break_before_block(role: TextRole, previous: Option<&BlockRecord>) -> bool {
    is_major_heading(role)
        && previous
            .map(|record| !is_major_heading(record.role))
            .unwrap_or(false)
}

fn is_major_heading(role: TextRole) -> bool {
    matches!(role, TextRole::Heading1 | TextRole::Heading2)
}

fn copy_string<const N: usize>(out: &mut String<N>, value: &str) {
    out.clear();
    for ch in value.chars() {
        if out.push(ch).is_err() {
            break;
        }
    }
}

fn proto_style_for_display_style(style: FontStyle) -> proto::text::FontStyle {
    match style {
        FontStyle::Regular => proto::text::FontStyle::Regular,
        FontStyle::Italic => proto::text::FontStyle::Italic,
        FontStyle::Bold => proto::text::FontStyle::Bold,
        FontStyle::BoldItalic => proto::text::FontStyle::BoldItalic,
    }
}

impl ui::reading::ReadingBlocks for ReaderStore {
    fn block_count(&self) -> usize {
        self.block_count
    }

    fn block(&self, index: usize) -> Option<BlockRecord> {
        self.block_record(index)
    }

    fn block_text(&self, index: usize) -> &str {
        ReaderStore::block_text(self, index)
    }

    fn block_style(&self, index: usize) -> FontStyle {
        ReaderStore::block_style(self, index)
    }

    fn page_break_before(&self, index: usize) -> bool {
        self.block_page_break_before
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    fn paragraph_end(&self, index: usize) -> bool {
        self.block_paragraph_end.get(index).copied().unwrap_or(true)
    }

    fn paragraph_start(&self, index: usize) -> bool {
        self.block_paragraph_start
            .get(index)
            .copied()
            .unwrap_or(false)
    }

    fn type_settings(&self) -> TypeSettings {
        self.type_settings
    }

    fn page_box(&self) -> ui::reading::PageBox {
        ReaderStore::page_box(self)
    }
}

pub(crate) fn chapter_pages_for_event(store: &ReaderStore) -> [u16; MAX_SD_CHAPTERS] {
    let mut pages = [0u16; MAX_SD_CHAPTERS];
    if store.toc_count > 0 {
        let count = store
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize);
        pages[..count].copy_from_slice(&store.toc_page[..count]);
    } else {
        let count = store
            .book_sections
            .len()
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize);
        for (page, section) in pages.iter_mut().zip(store.book_sections.iter().take(count)) {
            *page = section.start_page.min(u16::MAX as u32) as u16;
        }
    }
    pages
}

pub(crate) fn source_hash(path: &str, byte_size: u32) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in path.bytes().chain(byte_size.to_le_bytes()) {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}
