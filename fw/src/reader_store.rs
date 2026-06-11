use app_core::{ReaderSource, MAX_SD_CHAPTERS};
use display::font::FontStyle;
use heapless::String;
use proto::cache::{
    BlockRecord, BookV2SectionRecord, PageRecord, TocRecord, CACHE_KEY_BYTES, COVER_BYTES,
    COVER_HEIGHT, COVER_STRIDE, COVER_WIDTH,
};
use proto::text::{TextAlign, TextRole};

pub(crate) const MAX_LIBRARY_BOOKS: usize = 16;
pub(crate) const MAX_SD_TOC_ITEMS: usize = 128;
pub(crate) const MAX_BOOK_SECTIONS: usize = 160;
const MAX_PUBLISHED_CHAPTER_EVENTS: usize = MAX_SD_CHAPTERS;
// Titles only (hrefs/anchors are no longer stored), so 4 KB covers the full
// 128-item TOC at ~32 bytes per title and the saved RAM widens the stack
// region, which the EPUB open path runs close to.
pub(crate) const MAX_SD_TOC_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_READER_BLOCKS: usize = 384;
pub(crate) const MAX_READER_PAGES: usize = 96;
pub(crate) const MAX_READER_TEXT_BYTES: usize = 16_384;
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
pub(crate) const EMPTY_BOOK_SECTION_RECORD: BookV2SectionRecord = BookV2SectionRecord {
    section: 0,
    spine: 0,
    start_page: 0,
    page_count: 0,
    partial: false,
};

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
    pub(crate) entries: [LibraryBookEntry; MAX_LIBRARY_BOOKS],
    pub(crate) count: usize,
    pub(crate) current_index: Option<usize>,
    pub(crate) loaded_index: Option<usize>,
    pub(crate) loaded_chapter: u8,
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
    pub(crate) book_section_count: usize,
    pub(crate) book_sections: [BookV2SectionRecord; MAX_BOOK_SECTIONS],
    pub(crate) toc_text: [u8; MAX_SD_TOC_TEXT_BYTES],
    pub(crate) toc_text_len: usize,
    pub(crate) toc: [TocRecord; MAX_SD_TOC_ITEMS],
    pub(crate) toc_page: [u16; MAX_SD_TOC_ITEMS],
    pub(crate) toc_count: usize,
    pub(crate) text: [u8; MAX_READER_TEXT_BYTES],
    pub(crate) text_len: usize,
    pub(crate) blocks: [BlockRecord; MAX_READER_BLOCKS],
    pub(crate) block_styles: [FontStyle; MAX_READER_BLOCKS],
    pub(crate) block_spine: [u16; MAX_READER_BLOCKS],
    pub(crate) block_page_break_before: [bool; MAX_READER_BLOCKS],
    pub(crate) block_paragraph_end: [bool; MAX_READER_BLOCKS],
    pub(crate) block_count: usize,
    pub(crate) pages: [PageRecord; MAX_READER_PAGES],
    pub(crate) page_spine: [u16; MAX_READER_PAGES],
    pub(crate) page_count: usize,
}

impl ReaderStore {
    pub(crate) const fn new() -> Self {
        Self {
            status: LibraryScanStatus::NotScanned,
            entries: [const { LibraryBookEntry::new() }; MAX_LIBRARY_BOOKS],
            count: 0,
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
            book_section_count: 0,
            book_sections: [EMPTY_BOOK_SECTION_RECORD; MAX_BOOK_SECTIONS],
            toc_text: [0; MAX_SD_TOC_TEXT_BYTES],
            toc_text_len: 0,
            toc: [EMPTY_TOC_RECORD; MAX_SD_TOC_ITEMS],
            toc_page: [0; MAX_SD_TOC_ITEMS],
            toc_count: 0,
            text: [0; MAX_READER_TEXT_BYTES],
            text_len: 0,
            blocks: [EMPTY_BLOCK_RECORD; MAX_READER_BLOCKS],
            block_styles: [FontStyle::Regular; MAX_READER_BLOCKS],
            block_spine: [0; MAX_READER_BLOCKS],
            block_page_break_before: [false; MAX_READER_BLOCKS],
            block_paragraph_end: [true; MAX_READER_BLOCKS],
            block_count: 0,
            pages: [EMPTY_PAGE_RECORD; MAX_READER_PAGES],
            page_spine: [0; MAX_READER_PAGES],
            page_count: 0,
        }
    }

    pub(crate) fn clear_catalog(&mut self) {
        self.count = 0;
        for entry in self.entries.iter_mut() {
            entry.display_name.clear();
            entry.display_label.clear();
            entry.open_name.clear();
            entry.in_books_dir = false;
            entry.byte_size = 0;
            entry.source_hash = 0;
        }
        self.current_index = None;
    }

    pub(crate) fn catalog_count(&self) -> usize {
        self.count
    }

    pub(crate) fn catalog_count_u8(&self) -> u8 {
        self.count.min(u8::MAX as usize) as u8
    }

    pub(crate) fn catalog_is_empty(&self) -> bool {
        self.count == 0
    }

    pub(crate) fn catalog_entries(&self) -> &[LibraryBookEntry] {
        &self.entries[..self.count]
    }

    pub(crate) fn catalog_entry(&self, index: usize) -> Option<&LibraryBookEntry> {
        self.entries.get(index).filter(|_| index < self.count)
    }

    pub(crate) fn set_catalog_entry_source_hash(&mut self, index: usize, source_hash: u32) {
        if let Some(entry) = self.entries.get_mut(index).filter(|_| index < self.count) {
            entry.source_hash = source_hash;
        }
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
        }
        for (index, page) in self.pages.iter_mut().enumerate() {
            *page = EMPTY_PAGE_RECORD;
            self.page_spine[index] = 0;
        }
    }

    pub(crate) fn clear_book_index(&mut self) {
        self.book_total_pages = 0;
        self.current_section_start_page = 0;
        self.current_section_page_count = 0;
        self.book_cache_ready = false;
        self.book_cache_partial = false;
        self.book_section_count = 0;
        for record in self.book_sections.iter_mut() {
            *record = EMPTY_BOOK_SECTION_RECORD;
        }
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

    pub(crate) fn finish_book_load(&mut self, index: usize, chapter: u8, status: BookLoadStatus) {
        if matches!(status, BookLoadStatus::Ready) {
            self.set_current_index(index);
        }
        if matches!(status, BookLoadStatus::Ready | BookLoadStatus::Error) {
            self.loaded_index = Some(index);
            self.loaded_chapter = chapter;
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
        self.current_section_page_count = page_count.min(u16::MAX as usize) as u16;
    }

    pub(crate) fn set_section_partial(&mut self, partial: bool) {
        self.section_partial = partial;
    }

    pub(crate) fn set_cached_spine(&mut self, spine: u16) {
        self.cached_spine = spine;
    }

    pub(crate) fn set_book_index(
        &mut self,
        total_pages: u32,
        partial: bool,
        sections: &[BookV2SectionRecord],
    ) {
        self.book_total_pages = total_pages.max(1);
        self.book_cache_ready = true;
        self.book_cache_partial = partial;
        self.book_section_count = sections.len().min(self.book_sections.len());
        for (index, record) in sections.iter().take(self.book_section_count).enumerate() {
            self.book_sections[index] = *record;
        }
        for index in self.book_section_count..self.book_sections.len() {
            self.book_sections[index] = EMPTY_BOOK_SECTION_RECORD;
        }
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
        self.loaded_index == Some(index)
            && matches!(self.reader_status, BookLoadStatus::Ready)
            && !self.section_partial
            && self.page_count > 0
            && global_page >= self.current_section_start_page
            && global_page
                < self
                    .current_section_start_page
                    .saturating_add(self.current_section_page_count as u32)
    }

    pub(crate) fn local_page_for_global(&self, global_page: u32) -> usize {
        global_page
            .saturating_sub(self.current_section_start_page)
            .min(self.current_section_page_count.saturating_sub(1) as u32) as usize
    }

    pub(crate) fn section_for_global_page(&self, global_page: u32) -> Option<BookV2SectionRecord> {
        self.book_sections
            .iter()
            .take(self.book_section_count)
            .copied()
            .find(|section| {
                let start = section.start_page;
                let end = start.saturating_add(section.page_count.max(1) as u32);
                global_page >= start && global_page < end
            })
            .or_else(|| {
                self.book_sections
                    .iter()
                    .take(self.book_section_count)
                    .copied()
                    .last()
                    .filter(|section| global_page >= section.start_page)
            })
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
        Some(TocItem {
            title: self.toc_title(index),
            level: self.toc[index].level.max(1),
            page: u32::from(self.toc_page[index]) + 1,
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

    pub(crate) fn chapter_count_for_ui(&self) -> u8 {
        if self.toc_count > 0 {
            self.toc_count
                .min(MAX_PUBLISHED_CHAPTER_EVENTS)
                .min(u8::MAX as usize)
                .max(1) as u8
        } else {
            self.book_section_count
                .min(MAX_PUBLISHED_CHAPTER_EVENTS)
                .min(u8::MAX as usize)
                .max(1) as u8
        }
    }

    pub(crate) fn push(
        &mut self,
        display_name: &str,
        open_name: &str,
        in_books_dir: bool,
        byte_size: u32,
    ) {
        if self.count >= self.entries.len() {
            return;
        }
        let entry = &mut self.entries[self.count];
        entry.display_name.clear();
        entry.display_label.clear();
        entry.open_name.clear();
        let _ = entry.display_name.push_str(display_name);
        push_catalog_label(display_name, open_name, &mut entry.display_label);
        let _ = entry.open_name.push_str(open_name);
        entry.in_books_dir = in_books_dir;
        entry.byte_size = byte_size;
        entry.source_hash = source_hash(display_name, byte_size);
        self.count += 1;
    }

    pub(crate) fn set_current_index(&mut self, index: usize) {
        if index < self.count {
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
        let line = line.trim();
        if line.is_empty() || self.block_count >= self.blocks.len() {
            return true;
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
        self.block_count += 1;
        true
    }
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
    push_pretty_file_stem(stem, out);
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

fn push_pretty_file_stem(stem: &str, out: &mut String<64>) {
    let mut capitalize_next = true;
    for byte in stem.bytes() {
        let ch = match byte {
            b'-' | b'_' => {
                capitalize_next = true;
                b' '
            }
            b'a'..=b'z' if capitalize_next => {
                capitalize_next = false;
                byte - b'a' + b'A'
            }
            b'A'..=b'Z' | b'0'..=b'9' => {
                capitalize_next = false;
                byte
            }
            b'.' => break,
            _ => byte,
        };
        if ch == b' ' && out.as_str().ends_with(' ') {
            continue;
        }
        let _ = out.push(ch as char);
    }
    while out.as_str().ends_with(' ') {
        out.pop();
    }
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
        self.block_paragraph_end
            .get(index)
            .copied()
            .unwrap_or(true)
    }
}

pub(crate) fn chapter_pages_for_event(store: &ReaderStore) -> [u16; MAX_SD_CHAPTERS] {
    let mut pages = [0u16; MAX_SD_CHAPTERS];
    if store.toc_count > 0 {
        for index in 0..store
            .toc_count
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize)
        {
            pages[index] = store.toc_page[index];
        }
    } else {
        for index in 0..store
            .book_section_count
            .min(MAX_SD_TOC_ITEMS)
            .min(MAX_PUBLISHED_CHAPTER_EVENTS)
            .min(u8::MAX as usize)
        {
            pages[index] = store.book_sections[index].start_page.min(u16::MAX as u32) as u16;
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
