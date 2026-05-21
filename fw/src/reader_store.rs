use crate::{LibraryEvent, LIBRARY_EVENTS};
use display::font::FontStyle;
use heapless::String;
use proto::cache::{BlockRecord, PageRecord, TocRecord};
use proto::text::{TextAlign, TextRole};

pub(crate) const MAX_LIBRARY_BOOKS: usize = 8;
pub(crate) const MAX_SD_TOC_ITEMS: usize = 64;
pub(crate) const MAX_SD_TOC_TEXT_BYTES: usize = 4096;
pub(crate) const MAX_READER_BLOCKS: usize = 384;
pub(crate) const MAX_READER_PAGES: usize = 96;
pub(crate) const MAX_READER_TEXT_BYTES: usize = 16_384;
pub(crate) const MAX_READER_BLOCK_TEXT: usize = 768;
pub(crate) const CACHE_KEY_BYTES: usize = 8;
pub(crate) const COVER_WIDTH: usize = 202;
pub(crate) const COVER_HEIGHT: usize = 303;
pub(crate) const COVER_STRIDE: usize = (COVER_WIDTH + 7) / 8;
pub(crate) const COVER_BYTES: usize = COVER_STRIDE * COVER_HEIGHT;
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
    pub(crate) open_name: String<16>,
    pub(crate) in_books_dir: bool,
}

impl LibraryBookEntry {
    pub(crate) fn new() -> Self {
        Self {
            display_name: String::new(),
            open_name: String::new(),
            in_books_dir: false,
        }
    }
}

pub(crate) struct ReaderStore {
    pub(crate) status: LibraryScanStatus,
    pub(crate) entries: [LibraryBookEntry; MAX_LIBRARY_BOOKS],
    pub(crate) count: usize,
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
    pub(crate) fn new() -> Self {
        Self {
            status: LibraryScanStatus::NotScanned,
            entries: core::array::from_fn(|_| LibraryBookEntry::new()),
            count: 0,
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

    pub(crate) fn clear(&mut self) {
        self.count = 0;
        for entry in self.entries.iter_mut() {
            entry.display_name.clear();
            entry.open_name.clear();
            entry.in_books_dir = false;
        }
        self.loaded_index = None;
        self.loaded_chapter = 0;
        self.reader_status = BookLoadStatus::Empty;
        self.title.clear();
        self.author.clear();
        self.error.clear();
        self.cache_key.clear();
        self.clear_cover();
        self.cached_spine = 0;
        self.section_partial = false;
        self.clear_toc();
        self.clear_lines();
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

    pub(crate) fn force_next_block_to_new_page(&mut self) {
        if self.block_count < self.block_page_break_before.len() && self.block_count > 0 {
            self.block_page_break_before[self.block_count] = true;
        }
    }

    pub(crate) fn block_text(&self, index: usize) -> &str {
        let Some(record) = self.blocks.get(index) else {
            return "";
        };
        let start = record.text_offset as usize;
        let end = start.saturating_add(record.text_len as usize);
        core::str::from_utf8(self.text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn toc_title(&self, index: usize) -> &str {
        let Some(record) = self.toc.get(index) else {
            return "";
        };
        let start = record.title_offset as usize;
        let end = start.saturating_add(record.title_len as usize);
        core::str::from_utf8(self.toc_text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn toc_href(&self, index: usize) -> &str {
        let Some(record) = self.toc.get(index) else {
            return "";
        };
        let start = record.href_offset as usize;
        let end = start.saturating_add(record.href_len as usize);
        core::str::from_utf8(self.toc_text.get(start..end).unwrap_or(&[])).unwrap_or("")
    }

    pub(crate) fn push_toc_record(
        &mut self,
        title: &str,
        href: &str,
        level: u8,
        spine_index: i16,
    ) -> bool {
        if self.toc_count >= self.toc.len() {
            return false;
        }
        let title = title.trim();
        let href = strip_fragment(href).trim();
        if title.is_empty() || href.is_empty() {
            return true;
        }
        let title_start = self.toc_text_len;
        let title_bytes = title.as_bytes();
        let href_start = title_start.saturating_add(title_bytes.len());
        let href_bytes = href.as_bytes();
        let end = href_start.saturating_add(href_bytes.len());
        if end > self.toc_text.len()
            || title_bytes.len() > u16::MAX as usize
            || href_bytes.len() > u16::MAX as usize
        {
            return false;
        }
        self.toc_text[title_start..href_start].copy_from_slice(title_bytes);
        self.toc_text[href_start..end].copy_from_slice(href_bytes);
        self.toc_text_len = end;
        self.toc[self.toc_count] = TocRecord {
            title_offset: title_start as u32,
            title_len: title_bytes.len() as u16,
            href_offset: href_start as u32,
            href_len: href_bytes.len() as u16,
            anchor_offset: 0,
            anchor_len: 0,
            level: level.max(1),
            spine_index,
        };
        self.toc_count += 1;
        true
    }

    pub(crate) fn chapter_count_for_ui(&self) -> u8 {
        self.toc_count
            .max(self.page_count)
            .min(u8::MAX as usize)
            .max(1) as u8
    }

    pub(crate) fn push(&mut self, display_name: &str, open_name: &str, in_books_dir: bool) {
        if self.count >= self.entries.len() {
            return;
        }
        let entry = &mut self.entries[self.count];
        entry.display_name.clear();
        entry.open_name.clear();
        let _ = entry.display_name.push_str(display_name);
        let _ = entry.open_name.push_str(open_name);
        entry.in_books_dir = in_books_dir;
        self.count += 1;
    }
}

pub(crate) fn publish_chapter_pages(book_id: u32, store: &ReaderStore) {
    if store.toc_count > 0 {
        for index in 0..store.toc_count.min(MAX_SD_TOC_ITEMS).min(u8::MAX as usize) {
            let _ = LIBRARY_EVENTS.try_send(LibraryEvent::ChapterPage {
                book_id,
                chapter: index as u8,
                page: store.toc_page[index] as u32,
            });
        }
    } else {
        for index in 0..store.page_count.min(MAX_SD_TOC_ITEMS).min(u8::MAX as usize) {
            let _ = LIBRARY_EVENTS.try_send(LibraryEvent::ChapterPage {
                book_id,
                chapter: index as u8,
                page: index as u32,
            });
        }
    }
}

fn strip_fragment(value: &str) -> &str {
    value.split('#').next().unwrap_or(value)
}
