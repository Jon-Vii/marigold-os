//! Firmware side of the reader page plan: the store-backed fast paths
//! (cached page records, section-relative page mapping) and TOC page
//! targets over the bounded [`ReaderStore`]. Heights, pagination walks,
//! and page-body drawing live in [`ui::reading`] behind the
//! `ReadingBlocks` trait so firmware and host tools render one way.

use crate::reader_store::ReaderStore;
pub(crate) use display::font::{style_marker_code, STYLE_MARKER};
use proto::cache::PageRecord;
use ui::reading::{block_height, page_record_at, paginate_block_pages, ReadingBlocks};
pub(crate) use ui::reading::{
    first_styled_line_style, paragraph_indent, reader_layout_config, reader_x_for,
    READER_FOOTER_BASELINE_Y, READER_LEFT_X, READER_PAGE_BOTTOM, READER_PAGE_TOP, READER_RIGHT_X,
    READER_WRAP_SAFETY,
};

pub(crate) struct ReaderPagePlan {
    page_count: u32,
    page: PageRecord,
}

impl ReaderPagePlan {
    pub(crate) fn new(sd_library: &ReaderStore, requested_page: u32) -> Self {
        let page_count = reader_page_count(sd_library, READER_PAGE_TOP, READER_PAGE_BOTTOM);
        let requested_page = sd_library.local_page_for_global(requested_page.min(page_count - 1));
        let page = reader_page_at(
            sd_library,
            requested_page,
            READER_PAGE_TOP,
            READER_PAGE_BOTTOM,
        );
        Self { page_count, page }
    }

    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    pub(crate) fn page(&self) -> PageRecord {
        self.page
    }
}

pub(crate) fn reader_page_count(sd_library: &ReaderStore, page_top: i16, page_bottom: i16) -> u32 {
    if sd_library.book_total_pages > 0 {
        return sd_library.book_total_pages;
    }
    if sd_library.page_count > 0 {
        return sd_library.page_count as u32;
    }
    paginate_block_pages(sd_library, page_top, page_bottom).max(1) as u32
}

pub(crate) fn reader_page_at(
    sd_library: &ReaderStore,
    page_index: usize,
    page_top: i16,
    page_bottom: i16,
) -> PageRecord {
    if page_index < sd_library.page_count {
        return sd_library.pages[page_index];
    }
    page_record_at(sd_library, page_index, page_top, page_bottom)
}

pub(crate) fn rebuild_page_index(library: &mut ReaderStore, page_top: i16, page_bottom: i16) {
    library.page_count = 0;
    if library.block_count == 0 {
        return;
    }

    let mut first_block = 0usize;
    let mut block_count = 0usize;
    let mut y = page_top;

    for index in 0..library.block_count {
        let height = block_height(library, index);
        let new_page = (y + height > page_bottom
            || ReadingBlocks::page_break_before(library, index))
            && y > page_top;
        if new_page {
            push_sd_page_record(library, first_block, block_count);
            first_block = index;
            block_count = 0;
            y = page_top;
        }
        block_count += 1;
        y += height;
    }

    push_sd_page_record(library, first_block, block_count);
}

pub(crate) fn rebuild_toc_page_targets(library: &mut ReaderStore) {
    for toc_index in 0..library.toc_count {
        let spine_index = library.toc[toc_index].spine_index;
        if spine_index < 0 {
            library.toc_page[toc_index] = 0;
            continue;
        }
        let spine = spine_index as u16;
        let page = library
            .book_sections
            .iter()
            .take(library.book_section_count)
            .find(|section| section.spine == spine)
            .map(|section| section.start_page as usize)
            .or_else(|| {
                library
                    .page_spine
                    .iter()
                    .take(library.page_count)
                    .position(|page_spine| *page_spine == spine)
            })
            .unwrap_or(0);
        library.toc_page[toc_index] = page.min(u16::MAX as usize) as u16;
    }
}

fn push_sd_page_record(library: &mut ReaderStore, first_block: usize, block_count: usize) {
    if block_count == 0 || library.page_count >= library.pages.len() {
        return;
    }
    let page_index = library.page_count;
    library.pages[library.page_count] = PageRecord {
        first_block: first_block as u16,
        block_count: block_count as u16,
    };
    library.page_spine[page_index] = library.block_spine.get(first_block).copied().unwrap_or(0);
    library.page_count += 1;
}
